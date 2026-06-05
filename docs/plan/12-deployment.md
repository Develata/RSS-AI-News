# 12 — 部署

本章详解部署形态：Docker multi-stage 镜像 / scheduler 容器 / GHCR 发布 / CI / PostgreSQL 部署切换。

宪法 §1 锁定的 single-shot CLI 边界决定了部署不内置 cron，调度完全外化（详见
[../adr/0001-single-shot-cli-no-builtin-cron.md](../adr/0001-single-shot-cli-no-builtin-cron.md)）。

## 1. 边界

本章覆盖：
- Dockerfile multi-stage 结构与 cache 策略
- runtime 镜像 vs scheduler 镜像的二元发布
- supercronic 外挂调度形态（crontab 文件 / env 单行）
- GHCR 镜像命名 + 版本规则
- CI 四 job 矩阵
- SQLite → PostgreSQL 切换路径

**不覆盖**：
- 本地开发（`cargo build` / `cargo test`）→ [../operations/local-dev.md](../operations/local-dev.md)
- 运维指令（`migrate` / `doctor` 怎么跑）→ [../operations/](../operations/)
- 多方言 storage 内部 → [./05-storage.md](./05-storage.md)

## 2. 二元镜像发布

CI release workflow（[`.github/workflows/release.yml`](../../.github/workflows/release.yml)）
push tag `v*` 时同时构建发布两套镜像：

| 镜像 | 入口 | 用途 |
|---|---|---|
| `ghcr.io/develata/rss-ai-news:<tag>` | `rss-ai-news` 二进制直跑 | 一次性命令（`migrate` / `ingest` / `doctor` 等） |
| `ghcr.io/develata/rss-ai-news-scheduler:<tag>` | `scheduler-entrypoint.sh` + supercronic | 常驻容器按 cron 触发 |

底层共享同一 builder stage；scheduler 镜像只在 runtime 镜像基础上多装 supercronic。

镜像标签规则（`docker/metadata-action`）：
- `vX.Y.Z` → `X.Y.Z` + `X.Y` + `X` + `latest`（仅 stable）
- `vX.Y.Z-rc.N` / `vX.Y.Z-alpha.N` → 仅完整版本号，**不**打 `latest`

## 3. Dockerfile multi-stage

入口：[`docker/Dockerfile`](../../docker/Dockerfile)。三层结构：

```text
deps    →  builder  →  runtime
              │
              └──→  scheduler
```

### 3.1 `deps` stage

- 仅 COPY 各 crate 的 `Cargo.toml` + `Cargo.lock` + 占位 `src/lib.rs`
- 运行 `cargo build --release --workspace --bin rss-ai-news`（允许失败）
- 目的：让 cargo 注册表 / git / target 缓存固化在这一层，源码改动不触发依赖重编译

### 3.2 `builder` stage

- 在 `deps` 基础上 COPY 真实源码
- 删除占位 lib.rs 的 fingerprint + 触碰所有 .rs 的 mtime，强制 cargo 重编译工作区 crate
- 不带 builtin 依赖编译 → 编出最终 release 二进制

### 3.3 `runtime` stage

- `debian:bookworm-slim` 基础
- 安装最小运行时依赖（`ca-certificates` 等）
- `COPY --from=builder /app/target/release/rss-ai-news /usr/local/bin/`
- `ENTRYPOINT ["rss-ai-news"]`

### 3.4 `scheduler` stage

- `FROM runtime`
- 额外安装 supercronic（aptible/supercronic）静态二进制
- `COPY docker/scheduler-entrypoint.sh /usr/local/bin/`
- `ENTRYPOINT ["scheduler-entrypoint.sh"]`

### 3.5 Rust 工具链

`RUST_TOOLCHAIN=1.94.0` 在 Dockerfile 与 `.github/workflows/{ci,release}.yml` 三处保持一致。
升级时三处同步改一次，避免 CI 通过但镜像构建失败的版本错位。

## 4. scheduler 入口

[`docker/scheduler-entrypoint.sh`](../../docker/scheduler-entrypoint.sh) 支持两种调度形态：

### 4.1 多行 crontab 文件（推荐）

`RSS_CRONTAB_FILE`（默认 `/app/crontab`）挂载一个 supercronic 兼容的 crontab：

```cron
0 */3 * * *  sh -c '/usr/local/bin/rss-ai-news --config-dir /app/configs ingest && /usr/local/bin/rss-ai-news --config-dir /app/configs ai-run'
30 21 * * *  /usr/local/bin/rss-ai-news --config-dir /app/configs publish
```

文件存在且非空 → 直接交给 supercronic，跳过 env 模式。
适合需要分离 ingest 节奏与 publish 节奏的场景。

### 4.2 单行 env 模式（向后兼容）

```env
RSS_CRON_SCHEDULE="0 */6 * * *"
RSS_CRON_COMMAND="run --max-batches 3"
```

entrypoint 在内存里拼一行 crontab 给 supercronic。适合简单场景。

### 4.3 日志

supercronic 把每个 cron job 的 stdout/stderr 透传到自身 stdout，
`docker logs <container>` 可见。

## 5. compose 部署形态

仓库提供两份参考 compose：

| 文件 | 形态 | 适用 |
|---|---|---|
| [`docker/docker-compose.yml`](../../docker/docker-compose.yml) | 一次性 + 宿主 cron | 简单部署，宿主接管调度 |
| [`docker/docker-compose.scheduler.yml`](../../docker/docker-compose.scheduler.yml) | 常驻 + supercronic | 1panel / 容器面板风格 |

两套 compose 的取舍 详见 `docker-compose.scheduler.yml` 顶部注释：
- 常驻：状态走 docker 原生，cron 改环境即可调整；多一个 supercronic 进程
- 一次性：cron job 失败由 1panel 计划任务可见性更好；需宿主侧维护 cron

无论哪种形态，**首次 migrate 都必须显式执行一次**：

```bash
docker run --rm --env-file .env \
  -v "$(pwd)/configs:/app/configs:ro" \
  -v "$(pwd)/data:/app/data" \
  ghcr.io/develata/rss-ai-news:0.3.0 \
  --config-dir /app/configs migrate run
```

scheduler 容器不会在启动时自动 migrate（避免脏数据 / 误升级风险）。

## 6. CI workflow

[`.github/workflows/ci.yml`](../../.github/workflows/ci.yml) 四 job 并行（无 `needs:` 链）：

| Job | 验证项 |
|---|---|
| `lint` | `cargo fmt --check` + `cargo clippy --workspace -- -D warnings` |
| `test` | `cargo test --workspace`（SQLite path） |
| `migrate` | postgres 服务 + `migrate run` 端到端 |
| `docker-build` | `docker buildx build` 构建（不 push）验证 Dockerfile 不烂 |

并发取消（`concurrency.cancel-in-progress: true`）：同分支后续 push 自动取消前一次 run。

CI 通过是 PR 合入 main 的硬门槛。文档变更也不豁免（hooks 触发 `cargo fmt`，详见
[../AGENTS.md](../AGENTS.md)）。

## 7. PostgreSQL 部署切换

切换路径（详见 [./05-storage.md](./05-storage.md) §多方言）：

```env
# .env
DATABASE_URL=postgres://user:pass@db:5432/rss_ai_news
# app.toml
[database]
driver = "postgres"
```

步骤：
1. 准备 PG 实例（compose 或托管）
2. `migrate run` 在新 driver 下执行 `migrations/postgres/*` 全量迁移
3. `doctor` 验证 driver / 连接 / migration 一致性
4. 正常运行业务子命令

**注意**：SQLite → PG 不是无损迁移；当前**没有**自动数据搬运工具，需手动 dump/restore
或重新抓取。详见 [../adr/0006-postgres-go-real-no-shrink.md](../adr/0006-postgres-go-real-no-shrink.md)：
PostgreSQL 走"实补"路线（与 SQLite 字段对齐），**不收缩**。

## 8. 启动期 secret 校验

容器启动时（任一 stage 任一 cron job）由 `validate-config` 守门：
- `[ai].enabled=true` 缺 `OPENAI_API_KEY` → exit 78（`ConfigError`）
- `[publish].github_owner+repo` 非空缺 `GITHUB_TOKEN` → exit 78（`ConfigError`）
- source URL 含 RSSHub 占位符缺 `RSSHUB_BASE_URL` → exit 78（`ConfigError`）

详见 [./06-config.md](./06-config.md) §validate-config。

## 9. 端口与挂载约定

| 路径 | 用途 | 推荐挂载 |
|---|---|---|
| `/app/configs` | `app.toml` + `categories/*.toml` | `:ro` |
| `/app/data` | SQLite db + artifacts 文件后端 | rw |
| `/app/output` | publish local target 输出 | rw |
| `/app/logs` | tracing 日轮转目标 | rw |
| `/app/crontab` | supercronic crontab（scheduler only） | `:ro` |

metrics 端口（`[observability].metrics_bind`）默认 `127.0.0.1:9090`，
容器内暴露请改为 `0.0.0.0:9090` 并 publish。

## 10. 当前实现入口

| 内容 | 路径 |
|---|---|
| Dockerfile multi-stage | [`docker/Dockerfile`](../../docker/Dockerfile) |
| scheduler entrypoint | [`docker/scheduler-entrypoint.sh`](../../docker/scheduler-entrypoint.sh) |
| 一次性 compose | [`docker/docker-compose.yml`](../../docker/docker-compose.yml) |
| 常驻 scheduler compose | [`docker/docker-compose.scheduler.yml`](../../docker/docker-compose.scheduler.yml) |
| CI workflow | [`.github/workflows/ci.yml`](../../.github/workflows/ci.yml) |
| Release workflow | [`.github/workflows/release.yml`](../../.github/workflows/release.yml) |
| 服务端参考配置 | [`server-configs/`](../../server-configs/) |
| PostgreSQL 迁移 | [`migrations/postgres/`](../../migrations/postgres/) |

代码路径过时时在 [../map/architecture-diff.md](../map/architecture-diff.md)登记漂移。
