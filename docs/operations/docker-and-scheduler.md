# Docker 镜像与外挂调度

## 镜像形态

CI release workflow（push tag `v*` 触发）同时发布两套镜像：

| 镜像 | tag 示例 | 入口 | 用途 |
|---|---|---|---|
| runtime | `ghcr.io/develata/rss-ai-news:0.4.0` | `rss-ai-news` 二进制直跑 | 一次性命令（migrate / ingest / doctor 等） |
| scheduler | `ghcr.io/develata/rss-ai-news-scheduler:0.4.0` | `scheduler-entrypoint.sh` + supercronic | 常驻容器按 cron 触发 |

镜像 tag 规则（`docker/metadata-action`）：
- 稳定版 `vX.Y.Z` → `X.Y.Z` + `X.Y` + `X` + `latest`
- 预发 `vX.Y.Z-rc.N` → 仅完整版本号，**不**打 `latest`

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

文件：[`docker/docker-compose.scheduler.yml`](../../docker/docker-compose.scheduler.yml)。

适合 1panel 风格容器面板部署。两种调度形态（优先级从高到低）：

### 形态 1：外挂 crontab 文件（推荐）

scheduler 容器入口由环境变量 `RSS_CRONTAB_FILE` 控制（默认 `/app/crontab`）。
**只要该路径存在且非空**，entrypoint 直接交给 supercronic 跑，**跳过**形态 2 的 env 模式。
若文件不存在或为空，则自动降级到形态 2。

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

entrypoint 在内存里拼一行 crontab 给 supercronic。适合简单场景。

## 首次 migrate

**任何**部署形态下，scheduler 容器**不**自动 migrate。首次部署必须显式跑一次：

```bash
docker run --rm --env-file .env \
  -v "$(pwd)/configs:/app/configs:ro" \
  -v "$(pwd)/data:/app/data" \
  ghcr.io/develata/rss-ai-news:0.4.0 \
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
