# Docker 镜像与外挂调度

## 镜像形态

CI release workflow（push tag `v*` 触发）同时发布两套镜像：

| 镜像 | tag 示例 | 入口 | 用途 |
|---|---|---|---|
| runtime | `ghcr.io/develata/rss-ai-news:0.10.4` | `rss-ai-news` 二进制直跑 | 一次性命令（migrate / ingest / doctor 等） |
| scheduler | `ghcr.io/develata/rss-ai-news:0.10.4-scheduler` | `scheduler-entrypoint.sh` + supercronic | 常驻容器按 cron 触发 |

GHCR 只发布 `linux/amd64`。Dockerfile 里的 cargo-chef / supercronic 同时固定了 amd64 与 arm64 的 SHA-256，
arm64 主机可本地构建（`docker build -f docker/Dockerfile --target runtime .`），或用 `--platform linux/amd64` 仿真运行。

镜像 tag 规则（详见 [./ci-and-release.md](./ci-and-release.md) §镜像 tag 规则）：
- 稳定版 `vX.Y.Z` → `X.Y.Z`；两镜像都推送成功后，最高 stable tag 才晋升 `latest`，`X.Y` 线内最高才晋升 `X.Y`；
  scheduler 为各 tag 加 `-scheduler`
- 预发 tag 不推镜像（release verify gate 只接受 `X.Y.Z`）
- workflow 不维护 `X` major-only alias

## 一次性 compose（宿主 cron 接管）

文件：[`docker/docker-compose.yml`](../../docker/docker-compose.yml)。

```bash
# 首次部署
mkdir rss-ai-news && cd rss-ai-news
curl -O https://raw.githubusercontent.com/Develata/RSS-AI-News/main/docker/docker-compose.yml
cp .env.example .env  # 编辑 OPENAI_API_KEY / GITHUB_TOKEN / RSSHUB_BASE_URL 等

# 用一次性容器初始化 DB
docker compose run --rm rss-ai-news --config-dir /app/configs migrate run

# 验证
docker compose run --rm rss-ai-news --config-dir /app/configs doctor

# 后续靠宿主 cron / systemd timer 调度
# 例如 crontab -e：
# 0 */3 * * *  docker compose -f /path/to/docker-compose.yml run --rm rss-ai-news --config-dir /app/configs ingest && \
#              docker compose -f /path/to/docker-compose.yml run --rm rss-ai-news --config-dir /app/configs ai-run
# 30 21 * * *  docker compose -f /path/to/docker-compose.yml run --rm rss-ai-news --config-dir /app/configs publish
```

## 常驻 scheduler compose（容器内置 cron）

文件：[`docker/docker-compose.scheduler.yml`](../../docker/docker-compose.scheduler.yml)。镜像版本由 `RSS_AI_NEWS_VERSION`
选择（`.env` 中设置；缺省为该文件随附的 release，当前 `0.10.4`）。

适合 1panel 风格容器面板部署。两种调度形态（优先级从高到低）：

### 形态 1：外挂 crontab 文件（推荐）

scheduler 容器入口由环境变量 `RSS_CRONTAB_FILE` 控制（默认 `/app/crontab`）。
**只要该路径是非空普通文件**，entrypoint 直接交给 supercronic 跑，**跳过**形态 2 的 env 模式。
若文件不存在或为空，则自动降级到形态 2。若该路径存在但不是普通文件——最常见的是 compose 挂载了一个宿主上
不存在的 `./crontab`，Docker 在容器里建出了同名目录——entrypoint 报错退出，而不是悄悄改用默认调度。

```bash
# 1. 准备 crontab 文件（supercronic 兼容格式）
cat > /opt/rss-ai-news/crontab <<'CRON'
0 */3 * * *  sh -c '/usr/local/bin/rss-ai-news --config-dir /app/configs ingest && /usr/local/bin/rss-ai-news --config-dir /app/configs ai-run'
30 21 * * *  /usr/local/bin/rss-ai-news --config-dir /app/configs publish
CRON

# 2. compose 文件挂载 /app/crontab
volumes:
  - ./crontab:/app/crontab:ro

# 3. 启动
docker compose -f docker-compose.scheduler.yml up -d
```

可通过 `RSS_CRONTAB_FILE=/custom/path` env 覆盖默认路径。

### 形态 2：单行 env 模式

```env
# .env
RSS_CRON_SCHEDULE="0 */6 * * *"
RSS_CRON_COMMAND="run --max-batches 3"
```

entrypoint 拼一行 `<RSS_CRON_SCHEDULE> /usr/local/bin/rss-ai-news --config-dir /app/configs <RSS_CRON_COMMAND>`
给 supercronic（它用 `/bin/sh -c` 执行命令部分）。`RSS_CRON_COMMAND` 按 shell 命令行书写，含空格的参数照常加引号，
例如 `RSS_CRON_COMMAND="rebuild-report --output 'daily report.md'"`；两个变量都不能含换行或回车。适合简单场景。

## 首次 migrate 与升级

迁移行为（以代码为准，见 `crates/cli/src/context_factory.rs` 的 `open_write_storage`）：
- scheduler entrypoint 自身**不** migrate；
- 任何**写命令**（`ingest`、`ai-run`、`publish`、`publish-all`、`run`、`backfill`、`reindex`（非 dry-run）、`decision-run`）打开数据库时都会先
  应用 pending migrations 并轮换 active config 版本，所以 cron 触发的第一条写命令会自动完成首次迁移；
- 只读命令（`doctor`、`recent-entries`、`replay`、`rebuild-report`、`reindex --dry-run`）不 migrate；遇到 pending migration 时
  `doctor` 跳过 deep scan，其余报错退出。

推荐做法：首次启动前与**每次升级镜像前**，用一次性 runtime 容器显式跑 `migrate run`（必要时再跑 `migrate check`），
让 schema 变更只在一处、带可见输出地发生，而不是混在某个 cron job 的日志里；`migrate run` 在仍有活动 reindex job 时
会拒绝执行。

```bash
# 与 scheduler 同一个 compose 服务：镜像版本（.env 的 RSS_AI_NEWS_VERSION，经 compose 插值）、挂载、网络与
# 环境变量都与常驻容器一致；只把 entrypoint 换成 CLI 本身。
docker compose -f docker-compose.scheduler.yml run --rm --no-deps \
  --entrypoint /usr/local/bin/rss-ai-news \
  rss-ai-news-scheduler --config-dir /app/configs migrate run
```

不用 compose 的 single-shot 部署才直接跑 runtime 镜像；此时镜像 tag 由宿主 shell 写死，`--env-file` 只把变量传进容器、
不参与镜像名展开，所以 tag 必须手动写成与 scheduler 相同的版本（R3-11）：

```bash
docker run --rm --env-file .env \
  -v "$(pwd)/configs:/app/configs:ro" \
  -v "$(pwd)/data:/app/data" \
  ghcr.io/develata/rss-ai-news:0.10.4 \
  --config-dir /app/configs migrate run
```

## 挂载约定

| 容器路径 | 用途 | 推荐挂载 |
|---|---|---|
| `/app/configs` | `app.toml` + `categories/*.toml` | `:ro` |
| `/app/data` | SQLite DB + artifacts 文件后端 | rw |
| `/app/output` | publish local target 输出 | rw |
| `/app/logs` | tracing 日轮转目标 | rw |
| `/app/crontab` | supercronic crontab（scheduler 才需要） | `:ro` |

## 端口

| 端口 | 来源 | 是否需 publish |
|---|---|---|
| `9090` | `--metrics-bind` 或 `[observability].metrics_bind` | 需要外部 Prometheus scrape 时 publish；默认绑 `127.0.0.1:9090` 不暴露 |

容器内暴露 metrics 时绑 `0.0.0.0:9090` 后用 `-p 9090:9090` publish。

## 日志查看

scheduler 把每个 cron job 的 stdout / stderr 透传到自身 stdout：

```bash
docker logs -f rss-ai-news-scheduler
```

## 镜像内 binary 位置

```text
/usr/local/bin/rss-ai-news           # runtime + scheduler 镜像都有
/usr/local/bin/supercronic           # 仅 scheduler 镜像
/usr/local/bin/scheduler-entrypoint.sh   # 仅 scheduler 镜像
```

## 相关文档

- 设计：[../plan/12-deployment.md](../plan/12-deployment.md)
- 边界约束（不内置 cron）：[../adr/0001-single-shot-cli-no-builtin-cron.md](../adr/0001-single-shot-cli-no-builtin-cron.md)
- scheduler 镜像验收：[../acceptance-cases/commands/scheduler.md](../acceptance-cases/commands/scheduler.md)
- PG 切换：[./postgres-deployment.md](./postgres-deployment.md)

## 构建与供应链（2026-09-10）

Dockerfile 使用真实源码、cargo build --release --locked 与 registry/git/target BuildKit cache mounts；
不再制造 stub、忽略 build 失败或删除 fingerprints。可执行文件复制到 mount 外再进入 runtime stage。
新增 crate 无需维护额外 COPY/stub 名单。docs-backup 不进入构建上下文。

同一 builder 的 cache mounts 可复用；GitHub Actions cache exporter 默认不保存 cache mount 内容，
新 runner 的冷构建仍可能重新编译依赖，不能声称与旧 dependency layer 命中性能相等。
参见 [Docker 官方说明](https://docs.docker.com/build/ci/github-actions/cache/)。不引入 cargo-chef 或 cache-dance。

Supercronic 固定 v0.2.49，amd64/arm64 SHA-256 来自官方 GitHub release asset.digest，并逐个下载核验。
entrypoint 不打印可能带密钥的完整 crontab/命令。本机 amd64 已测版本、合法/非法 cron 和 SIGTERM；
镜像 runtime/debug/scheduler 的既有 CI smoke 仍是容器验收入口，本地无 Docker 的结果不能替代它。
