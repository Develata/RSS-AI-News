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

`doctor` 检查配置、数据库、外部依赖和 artifact 状态。`--deep` 会额外扫描数据库不变量，数据量大时会更慢。

### 重新生成报告

```bash
rss-ai-news --config-dir configs --category ai rebuild-report --date 2026-05-18 --output output/ai-2026-05-18.md
```

该命令基于已冻结的发布快照重新渲染 Markdown，不重新抓取 RSS，也不重新调用 AI。

## 配置说明

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
github_path_prefix = "archive"
local_output_dir = "output"
```

并在 `.env` 中填写：

```dotenv
GITHUB_TOKEN=ghp_...
```

`--local-only` 可以临时强制只写本地，不校验 GitHub token。

### 关闭 AI 后直通发布

如果只想抓取并发布原始 feed 摘要，可以关闭 AI：

```toml
[ai]
enabled = false

[publish]
include_unscored = true
```

此模式下 `run` 会跳过 `ai-run`，`publish` 使用未评分文章生成报告。若 `include_unscored = false`，关闭 AI 后不会产生发布候选。

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
sqlite_path = "data/rss-ai-news.db"
max_connections = 8
busy_timeout_ms = 5000
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

SQLite 与 PostgreSQL 使用不同迁移目录，但共享迁移编号。

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

要么在 `.env` 中填写 `RSSHUB_BASE_URL`，要么把该 source 改成完整 URL 或禁用。

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

## 更多文档

- [配置 Schema](./docs/design/config-schema.md)
- [CLI 语义](./docs/design/cli-semantics.md)
- [存储多方言设计](./docs/design/storage-multi-dialect.md)
- [状态机](./docs/design/state-machine.md)
- [错误模型与可观测性](./docs/design/error-and-observability.md)
- [Replay 与 Artifact](./docs/design/replay-and-artifacts.md)
- [文档总览](./docs/README.md)

## License

MIT
