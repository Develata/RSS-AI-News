# RSS-AI-News

RSS-AI-News 是一个一次性运行的 RSS 新闻处理 CLI。它按外部调度触发，完成：

```text
抓取 RSS/Atom/JSON Feed -> 抓取正文 -> AI 摘要与筛选 -> 生成 Markdown 报告 -> 本地或 GitHub 发布
```

它不内置 cron，也不长期驻留。你可以用 Docker、cron、systemd timer、GitHub Actions 或 Kubernetes CronJob 定时调用它。

## 适合什么场景

- 定期收集一组 RSS 源中的新闻或论文动态。
- 用 OpenAI 兼容接口对文章做摘要、标签、评分和保留/过滤判断。
- 每天生成按分类归档的 Markdown 报告。
- 先发布到本地目录，确认稳定后再推送到 GitHub 仓库。
- 需要可重复执行、可恢复、可检查健康状态的单进程管线。

## 快速开始

### 1. 准备配置

```bash
cp .env.example .env
cp configs/app.toml.example configs/app.toml
cp configs/categories/ai.toml.example configs/categories/ai.toml
```

编辑 `.env`：

```dotenv
OPENAI_API_KEY=sk-...
OPENAI_BASE_URL=https://api.openai.com/v1

# 如果某个 source 使用 {RSSHUB} 占位符，则必须填写
RSSHUB_BASE_URL=https://rsshub.example.com

# 如果 RSSHub 开启 ACCESS_KEY，可填写；rsshub 源会在抓取时自动携带 ?key=...
RSSHUB_ACCESS_KEY=
```

编辑 `configs/categories/ai.toml`，按需替换分类名称、prompt 和订阅源：

```toml
[[sources]]
key = "openai-blog"
display_name = "OpenAI Blog"
feed_url = "https://openai.com/blog/rss.xml"
feed_kind = "rss"
priority = 10
enabled = true
```

支持的 `feed_kind`：`rss`、`atom`、`json_feed`、`rsshub`。

### 2. 选择运行方式

#### Docker 推荐

```bash
docker build -f docker/Dockerfile --target runtime -t rss-ai-news:runtime .

docker run --rm --env-file .env \
  -v "$PWD/configs:/app/configs:ro" \
  -v "$PWD/data:/app/data" \
  -v "$PWD/output:/app/output" \
  rss-ai-news:runtime --config-dir /app/configs validate-config

docker run --rm --env-file .env \
  -v "$PWD/configs:/app/configs:ro" \
  -v "$PWD/data:/app/data" \
  -v "$PWD/output:/app/output" \
  rss-ai-news:runtime --config-dir /app/configs migrate run

docker run --rm --env-file .env \
  -v "$PWD/configs:/app/configs:ro" \
  -v "$PWD/data:/app/data" \
  -v "$PWD/output:/app/output" \
  rss-ai-news:runtime --config-dir /app/configs run
```

也可以使用 compose：

```bash
docker compose --profile runtime -f docker/docker-compose.yml run --rm \
  rss-ai-news --config-dir /app/configs validate-config

docker compose --profile runtime -f docker/docker-compose.yml run --rm \
  rss-ai-news --config-dir /app/configs run
```

`debug` profile 带有更多诊断工具：

```bash
docker compose --profile debug -f docker/docker-compose.yml run --rm rss-ai-news-debug --help
```

#### 本地运行

需要 Rust 1.88+。

```bash
cargo build --release --bin rss-ai-news

./target/release/rss-ai-news --config-dir configs validate-config
./target/release/rss-ai-news --config-dir configs migrate run
./target/release/rss-ai-news --config-dir configs run
```

开发态也可以直接：

```bash
cargo run -- --config-dir configs validate-config
cargo run -- --config-dir configs run
```

## 第一次使用 Walkthrough

下面这套步骤覆盖从 clone 到产出首份报告的全流程，建议第一次使用时严格按顺序跑一遍。

### Step 1：准备产物目录

```bash
mkdir -p data output logs
```

- `data/`：SQLite 数据库 + artifact 原文缓存。
- `output/`：本地发布的 Markdown 报告。
- `logs/`：`--log-file` 写入的日志（可选）。

### Step 2：填写最小配置

```bash
cp .env.example .env
cp configs/app.toml.example configs/app.toml
cp configs/categories/ai.toml.example configs/categories/ai.toml
```

`.env` 至少写：

```dotenv
OPENAI_API_KEY=sk-...
OPENAI_BASE_URL=https://api.openai.com/v1
```

`configs/categories/ai.toml` 至少保留 1 个 `[[sources]]` enabled = true 的 feed。

### Step 3：校验配置

```bash
rss-ai-news --config-dir configs validate-config
```

这一步不连库、不发请求，只解析配置。成功后再继续，避免后续命令在更深的位置才报配置错。

### Step 4：初始化数据库

```bash
rss-ai-news --config-dir configs migrate run
rss-ai-news --config-dir configs migrate check
```

`migrate run` 幂等，重复执行只 apply 新增迁移。`migrate check` 不写库，仅断言迁移已对齐。

### Step 5：跑一次完整流程

```bash
rss-ai-news --config-dir configs run --max-batches 1
```

`--max-batches 1` 让首跑只抓一批，便于快速验证管线通路；产物：

- `data/rss-ai-news.db` 中累积新的 feed entries / articles
- `output/<rendered-path>.md`，路径由 `[publish.template].path_template` 渲染决定；默认示例为 `output/AI_ML/2026/20260518.md`
- stderr 打印结构化日志 + 最终结果摘要

如果想确认整体健康：

```bash
rss-ai-news --config-dir configs doctor
```

`doctor` 退出码非 0 时按提示修复后再上调度器。

## 常用命令

### 校验配置

```bash
rss-ai-news --config-dir configs validate-config
```

只读取配置和 `.env`，不连接数据库，不访问外部网络。适合在编辑配置后快速检查。

### 初始化或升级数据库

```bash
rss-ai-news --config-dir configs migrate run
rss-ai-news --config-dir configs migrate check
```

`migrate run` 应用未执行的迁移；`migrate check` 只检查迁移状态，不写库。

### 跑完整流程

```bash
rss-ai-news --config-dir configs run
```

等价于按顺序执行：

```bash
rss-ai-news --config-dir configs ingest
rss-ai-news --config-dir configs ai-run
rss-ai-news --config-dir configs publish
```

常用限制参数：

```bash
rss-ai-news --config-dir configs run --max-batches 3
rss-ai-news --config-dir configs run --ingest-batch-size 100 --ai-batch-size 20
```

### 分阶段执行

```bash
# 只抓取 feed 并提取正文
rss-ai-news --config-dir configs ingest --batch-size 50

# 只处理待 AI 分析的文章
rss-ai-news --config-dir configs ai-run --batch-size 20

# 只发布当天报告到本地目录
rss-ai-news --config-dir configs publish --local-only

# 发布指定日期
rss-ai-news --config-dir configs publish --date 2026-05-18 --local-only
```

### 健康检查

```bash
rss-ai-news --config-dir configs doctor
rss-ai-news --config-dir configs doctor --deep
```

`doctor` 检查配置、数据库、外部依赖和 artifact 状态。`--deep` 会额外扫描数据库不变量，数据量大时会更慢（大型 PG 库可能 10s+）。

`--output-format json` 时输出形如：

```json
{
  "status": "ok",
  "checks": [
    { "name": "config", "status": "ok" },
    { "name": "database", "status": "ok", "driver": "sqlite" },
    { "name": "artifact_store", "status": "ok" }
  ]
}
```

调度脚本可用 `jq '.status'` 判定健康，再决定要不要发邮件。

### 日志与排查

默认日志走 stderr，pretty 格式。排查问题时常用组合：

```bash
# debug 级别 + JSON 格式，写文件便于 grep / jq
rss-ai-news --config-dir configs \
  --log-level debug --log-format json --log-file logs/ingest.log \
  ingest --batch-size 10

# 在另一终端：
tail -f logs/ingest.log | jq 'select(.level=="ERROR" or .level=="WARN")'
```

`--log-format json` 输出 sqlx / reqwest / 业务模块的 structured fields，方便定位 feed url / article_id 维度的失败。

启用 Prometheus `/metrics`（W11 仅暴露空 registry，业务 counter / histogram 接入留 v0.2）：

```bash
rss-ai-news --config-dir configs --metrics-bind 127.0.0.1:9090 run &
curl http://127.0.0.1:9090/metrics
```

### 重新生成报告

```bash
rss-ai-news --config-dir configs --category ai rebuild-report --date 2026-05-18 --output output/ai-2026-05-18.md
```

该命令基于已冻结的发布快照重新渲染 Markdown，不重新抓取 RSS，也不重新调用 AI。

## 配置说明

> **完整字段定义、默认值理由、effective 覆盖规则**：见 [`docs/design/config-schema.md`](docs/design/config-schema.md)。该文档逐字段给出语义、作用阶段、与状态机的关系（例如 `ai.enabled × include_unscored` 真值表、`[http]` 并发预算分配、`[runtime].max_batches_per_run` 作用边界）。本节只覆盖文件分工、常用环境变量、最常调字段和典型场景——遇到 README 没讲到的字段，去 `config-schema.md` 查。

项目使用三类配置文件：

| 文件 | 用途 | 是否提交到 git |
|---|---|---|
| `.env` | 密钥、代理、数据库 URL 等环境敏感值 | 否 |
| `configs/app.toml` | 全局配置：数据库、HTTP、AI、发布、日志等 | 是 |
| `configs/categories/*.toml` | 分类、prompt、订阅源列表、分类级发布覆盖 | 是 |

### 必填或常用环境变量

| 变量 | 何时需要 |
|---|---|
| `OPENAI_API_KEY` | `[ai].enabled = true` 时必填 |
| `OPENAI_BASE_URL` | `[ai].enabled = true` 时必填；默认可用 `https://api.openai.com/v1` |
| `RSSHUB_BASE_URL` | 任一订阅源使用 `{RSSHUB}` 占位符时必填 |
| `RSSHUB_ACCESS_KEY` | 可选；设置后会给 `feed_kind = "rsshub"` 的源在抓取时追加访问 key，不写入 `feed_sources.feed_url` |
| `GITHUB_TOKEN` | 远端 GitHub 发布时必填 |
| `DATABASE_URL` | 使用 PostgreSQL 时必填 |
| `HTTP_PROXY` / `HTTPS_PROXY` | 需要进程级 HTTP 代理时填写 |

### 本地发布与远端发布

默认示例配置是本地发布：

```toml
[publish]
github_owner = ""
github_repo = ""
local_output_dir = "output"
```

此时 `publish` 会把 Markdown 写入本地 `output/` 目录。

如需发布到 GitHub，设置：

```toml
[publish]
github_owner = "your-name-or-org"
github_repo = "your-repo"
github_branch = "main"
github_path_prefix = "docs/news"
local_output_dir = "output"
```

并在 `.env` 中填写：

```dotenv
GITHUB_TOKEN=ghp_...
```

`--local-only` 可以临时强制只写本地，不校验 GitHub token。

远端最终路径按下面规则拼接：

```text
<github_path_prefix>/<[publish.template].path_template 渲染结果>
```

例如：

```toml
[category]
key = "ai_ml"
display_name = "科技新闻"

[publish]
github_path_prefix = "docs/news"

[publish.template]
path_template = "{CATEGORY_KEY}/{YYYY}/{YYYYMMDD}.md"
```

发布 `2026-01-03` 时，远端路径为：

```text
docs/news/AI_ML/2026/20260103.md
```

如果需要每个板块单独指定目录，可以在对应分类里覆盖路径模板：

```toml
[category.publish_override]
path_template = "math/{YYYY}/{YYYYMMDD}.md"
```

在上面的全局 `github_path_prefix = "docs/news"` 下，`math_research` 会发布到：

```text
docs/news/math/2026/20260103.md
```

如果每个分类要完全控制仓库内路径，可以把全局前缀置空，并在每个分类的
`path_template` 写完整相对路径：

```toml
[publish]
github_path_prefix = ""

[category.publish_override]
path_template = "docs/math/{YYYY}/{YYYYMMDD}.md"
```

### 发布路径与 Markdown 模板

`[publish.template]` 是必填配置段，用于控制报告路径和 Markdown 输出。示例配置默认是 VitePress 风格：

```toml
[publish.template]
path_template = "{CATEGORY_KEY}/{YYYY}/{YYYYMMDD}.md"
frontmatter_template = """
---
title: {date}
date: {date}
excerpt: {excerpt_yaml}
---
"""
report_template = """
{frontmatter}
# {title_md}
{excerpt_block}
{items}
"""
item_template = """
## {item_title_md}{score_badge}

{tags_block}- **Source:** `{source_code}` | [阅读原文]({url_md})

> [摘要]
{summary_blockquote}

---

"""
```

常用占位符：

| 模板字段 | 可用占位符 |
|---|---|
| `path_template` | `{category_key}`、`{CATEGORY_KEY}`、`{date}`、`{YYYY}`、`{MM}`、`{DD}`、`{YYYYMMDD}` |
| `frontmatter_template` | `{title}`、`{title_yaml}`、`{date}`、`{YYYY}`、`{MM}`、`{DD}`、`{YYYYMMDD}`、`{excerpt}`、`{excerpt_yaml}` |
| `report_template` | `{frontmatter}`、`{title}`、`{title_md}`、`{date}`、`{YYYY}`、`{MM}`、`{DD}`、`{YYYYMMDD}`、`{category_key}`、`{CATEGORY_KEY}`、`{category_display_name}`、`{category_display_name_md}`、`{excerpt}`、`{excerpt_yaml}`、`{excerpt_block}`、`{items}`、`{generated_at}` |
| `item_template` | `{item_title}`、`{item_title_md}`、`{score}`、`{score_badge}`、`{tags}`、`{tags_block}`、`{source}`、`{source_md}`、`{source_code}`、`{url}`、`{url_md}`、`{summary}`、`{summary_inline}`、`{summary_blockquote}` |

校验规则：

- `path_template` 必须渲染为相对路径，不能包含 `..`、反斜杠或绝对路径。
- 全局 `[publish.template].path_template` 必须包含分类占位符和日期占位符，避免不同分类或日期互相覆盖。
- 分类级 `[category.publish_override].path_template` 可以省略分类占位符，但仍必须包含日期占位符；`validate-config` 会以样本日期渲染所有分类的 effective path_template 并查重，发现跨分类重复就报错（即使语法合法）。
- `report_template` 必须包含 `{items}`。
- `item_template` 必须包含标题和摘要占位符。
- 未知占位符或未闭合 `{...}` 会在 `validate-config` 阶段失败。

### 关闭 AI 后直通发布

如果只想抓取并发布原始 feed 摘要，可以关闭 AI：

```toml
[ai]
enabled = false

[publish]
include_unscored = true
```

此模式下 `run` 会跳过 `ai-run`，`publish` 使用未评分文章生成报告。若 `include_unscored = false`，关闭 AI 后不会产生发布候选。

### 常调字段速查

`app.toml` 字段中实际部署时最常碰到的字段，按 section 分组。每项的完整语义、与状态机/并发模型的关系见 `docs/design/config-schema.md` 对应小节。

**网络与并发（`[http]`）**

| 字段 | 默认 | 何时调 |
|---|---|---|
| `timeout_seconds` | `30` | feed 源响应慢、超时较多时调到 `60`–`90`；过大会拖慢失败感知 |
| `concurrent_feeds` | `10` | ingest 阶段最大并发拉 feed 数；源很多（>50）时往上调，注意 RSSHub 等共享后端的限流 |
| `concurrent_fetches` | `5` | extract 阶段并发抓详情页；目标站点能承受时可调到 `10`+ |

**AI 调用（`[ai]` / `[ai.rate_limit]`）**

| 字段 | 默认 | 何时调 |
|---|---|---|
| `model` | `"gpt-4o-mini"` | 按你的 OpenAI 兼容 API 实际可用模型填；模型 ID 不存在会跑时报错 |
| `max_input_chars` | `8000` | 单篇文章送入 AI 前的字符截断；越大成本越高，越小可能丢上下文 |
| `request_timeout_seconds` | `60` | 慢模型 / 长输入要调到 `120`+；过短会让 AI 阶段大量 `timeout` 重试 |
| `ai.rate_limit.requests_per_minute` | `60` | 按你的 API tier 调；超出会被 governor 排队，不会丢请求 |

**发布筛选（`[publish]`）**

| 字段 | 默认 | 何时调 |
|---|---|---|
| `max_items_per_report` | `30` | 单份日报最多条数；摘要式分类调小（10–15），全量归档调大 |
| `min_importance_score` | `30` | AI 路径下入选门槛（0–100）；想看更少更精调 `50`+；`0` 表示"显式无下限"（与不写不同，详见 config-schema §4.1） |
| `candidate_window_hours` | `48` | 发布候选只取 `[now-Nh, now]`，按 `published_at`，缺失时退回 `discovered_at`；`0` 表示不限制下界，仍排除未来时间 |
| `include_unscored` | `false` | 关闭 AI 直通发布模式必须 `true`；AI 开启时该字段无效（详见 config-schema §4.1 真值表） |
| `template.path_template` | `"{CATEGORY_KEY}/{YYYY}/{YYYYMMDD}.md"` | 控制本地输出与 GitHub 远端相对路径；远端还会再加 `github_path_prefix` |

**运行边界（`[runtime]` / `[artifact]`）**

| 字段 | 默认 | 何时调 |
|---|---|---|
| `runtime.max_batches_per_run` | `10` | 单次 run 处理批次上限；积压多想一次清空时用 `--max-batches=0` 临时覆盖（仅 ingest/ai-run/run 三个子命令支持），不要直接改默认 |
| `artifact.retention_policy` | `"on_failure"` | 排错期改 `"always"`，让所有 raw artifact 都进 `raw_artifacts` 表供 `replay` 取；恢复后改回 `"on_failure"` 控成本 |

**未列出但需要知道的**：

- `[database].driver` —— `"sqlite"` 与 `"postgres"` 切换的所有影响（见 [`docs/design/storage-multi-dialect.md`](docs/design/storage-multi-dialect.md)）
- `[publish].github_owner` / `github_repo` —— 空字符串触发本地发布模式，详见上文「本地发布与远端发布」
- `[category.publish_override]` —— 分类级覆盖 `[publish]` 全局默认，按字段独立生效（详见 config-schema §4.5）

## 数据库

默认使用 SQLite：

```toml
[database]
driver = "sqlite"
sqlite_path = "data/rss-ai-news.db"
```

切换 PostgreSQL：

```toml
[database]
driver = "postgres"
sqlite_path = "data/rss-ai-news.db"   # 占位字段，driver=postgres 时被忽略
max_connections = 8
busy_timeout_ms = 5000                # 占位字段，driver=postgres 时被忽略
```

`.env`：

```dotenv
DATABASE_URL=postgres://user:pass@host:5432/dbname
```

然后执行：

```bash
rss-ai-news --config-dir configs migrate run
rss-ai-news --config-dir configs migrate check
```

切 PG 时几个常见坑：

- `driver = "postgres"` 必须配 `DATABASE_URL`，缺则 exit 78 ConfigError；URL 必须是 `postgres://` 或 `postgresql://` schema。
- W11 起 SQLite / PostgreSQL 走两套迁移目录，但共享版本号。从 SQLite 迁出已有数据需要走 `replay` + 手动导入（非自动 schema dump，参见 `docs/design/storage-multi-dialect.md`）。
- `max_connections=8` 是 SQLite 设置；PG pool 上限由 sqlx 内部 `PgPoolOptions::max_connections(env-tunable)` 控制，开发期不必动。
- 长跑过 SQLite WAL 后切回去之前，记得 `VACUUM` 一次，或者 backup 后重建 db 文件。

## 调度示例

### cron

```cron
15 8 * * * cd /path/to/RSS-AI-News && ./target/release/rss-ai-news --config-dir configs run >> logs/rss-ai-news.log 2>&1
```

### Docker + cron

```cron
15 8 * * * cd /path/to/RSS-AI-News && docker run --rm --env-file .env -v "$PWD/configs:/app/configs:ro" -v "$PWD/data:/app/data" -v "$PWD/output:/app/output" rss-ai-news:runtime --config-dir /app/configs run
```

调度器只需要重复调用 `run`。重复执行是预期用法；发布记录带幂等键，已完成的发布不会被静默覆盖。

### scheduler 镜像（一体化部署）

除了 runtime 镜像 + 宿主 cron 的搭配外，发版同时推一个 **scheduler 镜像**：

```
ghcr.io/develata/rss-ai-news:<version>            # runtime（single-shot CLI）
ghcr.io/develata/rss-ai-news:<version>-scheduler  # scheduler（常驻 + 内置 cron）
```

scheduler 镜像在 runtime 之上叠了一层 [supercronic](https://github.com/aptible/supercronic)：容器持续 Running，内部按 `RSS_CRON_SCHEDULE` 周期触发 `rss-ai-news <RSS_CRON_COMMAND>`。底层 binary 仍是 single-shot（blueprint §14 硬约束）；只是把「调度」职责从宿主搬进容器。

适用场景：在 1panel / Portainer 这类面板里希望容器一直 Running、不接外部 cron 的环境。一份 compose 参考见 `docker/docker-compose.scheduler.yml`。最少 `.env` 配置：

```dotenv
TIME_ZONE=Asia/Shanghai
DATABASE_URL=postgres://user:pass@host:5432/dbname
OPENAI_API_KEY=sk-...
OPENAI_BASE_URL=https://api.openai.com/v1
RSS_CRON_SCHEDULE=15 8 * * *       # 每天 08:15
RSS_CRON_COMMAND=run               # 完整 ingest -> ai-run -> publish
```

`docker compose up -d` 启动后，`docker logs -f rss-ai-news-scheduler` 看每次 cron 触发的输出。修改 `RSS_CRON_SCHEDULE` / `RSS_CRON_COMMAND` 后 `docker compose up -d` 重建容器即生效，**不需要重 build 镜像**。

#### 多行 crontab（分离抓取与推送节奏）

需要「一天多次抓取、推送只一次」之类的分离节奏时，挂一个 supercronic 兼容的 crontab 文件到 `/app/crontab`。entrypoint 检测到该文件非空就直接用它，自动忽略 `RSS_CRON_SCHEDULE` / `RSS_CRON_COMMAND`。

宿主放 `./crontab`：

```cron
# 每 3 小时抓取 + AI 评分
0 */3 * * * sh -c '/usr/local/bin/rss-ai-news --config-dir /app/configs ingest && /usr/local/bin/rss-ai-news --config-dir /app/configs ai-run'
# 每天 21:30 推送当日报
30 21 * * * /usr/local/bin/rss-ai-news --config-dir /app/configs publish
```

compose 里取消注释 `./crontab:/app/crontab:ro` 那行，`docker compose up -d` 重建即可。覆盖默认路径用 `RSS_CRONTAB_FILE` 环境变量。

`publish` 对 `(category, date, render_version)` 是幂等的——同日多次推送只走 `init: already_exists:published_*`，**不会重复 commit 到 GitHub**。但需要注意：第一次 publish 之后新抓的文章不会自动并入当日报告，所以 publish 应安排在最后一次抓取之后。

首次 migrate / 手动 doctor 仍建议用 runtime 镜像一次性跑（避免动 scheduler 容器调度节奏）：

```bash
docker run --rm --env-file .env \
  -v "$(pwd)/configs:/app/configs:ro" \
  -v "$(pwd)/data:/app/data" \
  ghcr.io/develata/rss-ai-news:0.1.0 \
  --config-dir /app/configs migrate run
```

## 输出目录结构

跑过 `run` 后预期看到：

```text
data/
  rss-ai-news.db                          # SQLite 数据库（driver=postgres 时此文件不会被创建）
output/
  <rendered-path>.md                      # 由 [publish.template].path_template 渲染
logs/
  *.log                                   # --log-file 指定时生成
```

默认示例模板下，`category.key = "ai_ml"` 且报告日期为 `2026-01-03` 时，本地路径为：

```text
output/AI_ML/2026/20260103.md
```

注意：

- v0.1.0 **raw artifact 全部走数据库 inline（SQLite BLOB / PG BYTEA）**，不落本地文件；`[artifact].file_storage_dir` 字段是 v0.2 large-payload 外置存储预留，当前未消费。`replay` 命令直接从 `raw_artifacts` 表读。
- `rebuild-report` 从数据库 `publish_record` / `publish_item` 表重渲染，不需要本地 snapshot 文件；删 `output/<category>/<date>.md` 不会影响重渲，只是看不到旧产物对比。
- Docker 部署时挂 `data/`（含 db + 未来 artifact 外置存储）+ `output/` 两个 volume 即可。

## 全局参数

```text
rss-ai-news [global flags] <command>
```

常用全局参数：

| 参数 | 说明 |
|---|---|
| `--config-dir <path>` / `-c <path>` | 配置目录，默认 `configs` |
| `--db-path <path>` | 覆盖 SQLite 数据库路径 |
| `--category <key>` / `-C <key>` | 只处理指定分类 |
| `--timezone <tz>` | 覆盖发布时区 |
| `--output-format pretty|json` | 控制最终结果输出格式 |
| `--log-level trace|debug|info|warn|error` | 控制日志级别 |
| `--log-format pretty|json` | 控制日志格式 |
| `--log-file <path>` | 写日志文件；空值表示只输出到 stderr |
| `--metrics-bind <addr>` | 启动 Prometheus `/metrics` 端点，例如 `127.0.0.1:9090` |

注意：全局 `--dry-run` 当前不是所有命令都支持。需要预演重建规则时优先使用 `reindex --dry-run`。

## 按场景选择子命令

| 我想做什么 | 用哪个命令 |
|---|---|
| 第一次部署，验证配置能跑通 | `validate-config` → `migrate run` → `run --max-batches 1` |
| 调度器每日跑一次完整流程 | `run`（不带 `--max-batches`） |
| 只想抓 feed 不想花钱跑 AI | 临时 `[ai].enabled = false` + `ingest` |
| 改了 source 列表想立刻拉新源 | `ingest` 单独跑一次 |
| 报告生成异常想看 raw feed / html | `replay --kind {feed\|html\|ai} --key <artifact_key>`（或 `--id <i64>`） |
| 改了去重 / 评分规则要重算历史 | `reindex --target {link_hash\|content_hash\|categories\|all} --dry-run` 先看影响面，再去掉 `--dry-run` 真跑 |
| 数据库刚导入历史，想补正文 / AI | `backfill --target extract` / `backfill --target ai` |
| 想重发同一天报告做版本对比 | `rebuild-report --date YYYY-MM-DD --output <新文件>` |
| 调度器报错时定位是配置还是网络 | `doctor` 看 status；exit 78 = 配置；exit 1 = 运行期 |

## 子命令速查

| 命令 | 用途 |
|---|---|
| `validate-config` | 校验 `.env`、`app.toml` 和 `categories/*.toml` |
| `migrate run` | 应用数据库迁移 |
| `migrate check` | 检查数据库迁移状态 |
| `ingest` | 抓取 feed、去重、抓正文、入库 |
| `ai-run` | 对待处理文章调用 AI 并写回结果 |
| `publish` | 选稿、冻结快照、渲染并发布报告 |
| `run` | 串联执行 `ingest -> ai-run -> publish` |
| `doctor` | 健康检查 |
| `rebuild-report` | 基于发布快照重新渲染报告 |
| `backfill` | 对历史数据补跑正文提取或 AI |
| `replay` | 从 raw artifact 回放 feed/html/AI 解析 |
| `reindex` | 重算派生字段或规则版本 |

查看完整参数：

```bash
rss-ai-news --help
rss-ai-news ingest --help
rss-ai-news publish --help
```

## 输出与退出码

默认输出人类可读摘要；需要机器读取时使用：

```bash
rss-ai-news --config-dir configs --output-format json run
```

退出码：

| Code | 含义 |
|---|---|
| `0` | 成功 |
| `1` | 运行时错误，例如网络、IO、数据库或远端调用失败 |
| `2` | 参数使用错误 |
| `78` | 配置错误 |

配置错误会尽量指出具体文件、字段和原因。调度脚本可以按退出码区分“配置需要人工修正”和“运行期临时失败”。

## 常见问题

### `validate-config` 报缺少 `RSSHUB_BASE_URL`

某个分类配置中使用了 `{RSSHUB}`：

```toml
feed_url = "{RSSHUB}/huggingface/daily-papers"
```

要么在 `.env` 中填写 `RSSHUB_BASE_URL`，要么把该 source 改成完整 URL 或禁用。若 RSSHub 开启了 `ACCESS_KEY`，推荐在 `.env` 填写 `RSSHUB_ACCESS_KEY`，配置文件中无需逐条写 `?key=...`；即使手写了 `key`，加载后也会从持久化 URL 中剥离，只在抓取请求中临时携带。

### `ai-run` 在关闭 AI 后返回配置错误

这是预期行为。`ai.enabled = false` 时，显式调用 `ai-run` 与配置意图矛盾，会返回 exit 78。使用 `run` 时会自动跳过 AI 阶段。

### 没有生成报告

优先检查：

1. `doctor` 是否通过。
2. 是否有成功抓取的文章。
3. `[ai].enabled = true` 时，AI 是否产生 `keep_decision = true` 的结果。
4. `min_importance_score` 是否过高。
5. `[ai].enabled = false` 时，`include_unscored` 是否为 `true`。

### 想重新发布同一天报告

普通重复执行 `publish` 会复用幂等记录并避免覆盖。确实需要重新生成新的发布批次时：

```bash
rss-ai-news --config-dir configs publish --date 2026-05-18 --force
```

`--force` 不会删除旧发布记录，而是生成新的发布 key。

### PG 连不上 / `PoolTimedOut`

按顺序排查：

1. `psql "$DATABASE_URL" -c 'SELECT 1'` 是否能连通；不能则先解决网络/防火墙/凭证。
2. `migrate run` 报 `PoolTimedOut` 通常是连接被中间件（如 PgBouncer transaction mode）拒了；本项目要求 session mode。
3. 用 `--log-level debug` 跑，日志会输出 sqlx connect error 详情，但**不会泄露密码**。

### `ai-run` 跑得很慢

每篇文章 1 次 OpenAI 调用，默认 `[ai].request_timeout_seconds = 60` 较保守。优化方向：

- 调大 `--ai-batch-size`（命令行单次批 size），同时让 `[ai.rate_limit].requests_per_minute` 留够预算。
- 换更快的模型（`[ai].model`，例如 `gpt-4o-mini` 替成更轻量的）。
- 减小 `[ai].max_input_chars` 让 prompt 更短，减少 token 耗时。
- 关闭 `[ai].enabled` 后只跑 `ingest`，待集中处理时再批量 `ai-run`。

### `--metrics-bind` 端口被占

`/metrics` 服务在 CLI 进程内起。同一调度时段若两个实例并发跑会冲突；要么换端口、要么用 `0.0.0.0:0` 让 OS 分配，要么干脆不暴露（W11 该 endpoint 是空 registry，业务侧 counter 尚未接入，移除 `--metrics-bind` 不会丢可观测性）。

### `docker run` 找不到 `/app/configs/categories/ai.toml`

容器内默认工作目录是 `/app`，`-v "$PWD/configs:/app/configs:ro"` 把宿主机 `configs/` 挂进去；如果本地目录结构不是 `configs/categories/*.toml`，请相应调整 mount 路径或 `--config-dir`。

### `validate-config` 报模板占位符错误

`[publish.template]` 只支持上文列出的占位符。常见错误：

- 写成 `{titel_md}`、`{summary_blockqoute}` 这类拼写错误。
- 模板里有未闭合的 `{` 或多余的 `}`。
- `path_template` 没有包含分类或日期，可能导致报告互相覆盖。
- `path_template` 包含 `../`、反斜杠或绝对路径。

修正后重新执行：

```bash
rss-ai-news --config-dir configs validate-config
```

## 当前版本状态（v0.1.0）

已实装：

- SQLite 与 PostgreSQL 双方言存储（参见 `docs/design/storage-multi-dialect.md`）。
- 10 个 CLI 子命令（含 `doctor` / `replay` / `reindex --dry-run`）。
- 可配置发布路径与 Markdown 模板（`[publish.template]`）。
- 双 backend `doctor` / `replay`。
- Prometheus `/metrics` HTTP endpoint（空 registry 占位，业务 counter 接入留 v0.2）。
- 双 GitHub Actions CI job：`cargo test`（默认 SQLite）+ `test (postgres)`（service container + `--test-threads=1`）。

v0.2 follow-up：

- 全局 `--dry-run` 全子命令覆盖（v0.1.0 仅 `reindex --dry-run`，其他子命令带 `--dry-run` 返 `DryRunNotImplemented` exit 1）。
- 业务侧 metrics counter / histogram 全栈接入。
- `replay` 输出格式扩展（v0.1.0 主要面向开发期诊断）。

## 更多文档

- [配置 Schema](./docs/design/config-schema.md)
- [CLI 语义](./docs/design/cli-semantics.md)
- [存储多方言设计](./docs/design/storage-multi-dialect.md)
- [状态机](./docs/design/state-machine.md)
- [错误模型与可观测性](./docs/design/error-and-observability.md)
- [Replay 与 Artifact](./docs/design/replay-and-artifacts.md)
- [本地开发：pre-commit hook 启用](./.githooks/README.md)
- [文档总览](./docs/README.md)

## License

MIT
