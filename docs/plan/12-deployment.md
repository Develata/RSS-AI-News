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
- CI 五 job 矩阵
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
| `ghcr.io/develata/rss-ai-news:<tag>-scheduler` | `scheduler-entrypoint.sh` + supercronic | 常驻容器按 cron 触发 |

底层共享同一 builder stage；scheduler 镜像只在 runtime 镜像基础上多装 supercronic。

GHCR 只发布 `linux/amd64` 镜像（release runner 为 amd64，不做 QEMU 交叉构建）。Dockerfile 为 cargo-chef 与 supercronic
同时固定了 amd64 / arm64 的 SHA-256，所以 arm64 主机可以本地 `docker build`；需要 arm64 镜像时在本机构建，或在
arm64 主机上以 `--platform linux/amd64` 仿真运行已发布镜像。

镜像标签规则：
- `vX.Y.Z` → 先推 `X.Y.Z`（及 `X.Y.Z-scheduler`）；两镜像都成功后，若为仓库最高 stable tag 再晋升 `latest`，
  若为 `X.Y` 线内最高再晋升 `X.Y`（各带 `-scheduler` 对应 tag）；重跑旧 tag 不会让它们回退。
  workflow 不维护 `X` major-only alias。
- prerelease tag（`vX.Y.Z-rc.N` 等）不推镜像：release verify gate 只接受 `X.Y.Z`。

## 3. Dockerfile multi-stage

入口：[`docker/Dockerfile`](../../docker/Dockerfile)。构建与运行 stage：

```text
builder  →  runtime  →  debug
                └──→  scheduler
```

### 3.1 `builder` stage

- 基于 `rust:1.97.0-slim-bookworm`（按 index digest 固定；digest 由 dependabot docker 生态刷新），直接 COPY 真实源码，
  不再构造占位源码或忽略构建失败。
- BuildKit cache mounts 复用 Cargo registry、git 与 target；执行 `cargo build --release --locked --bin rss-ai-news`。
- 将二进制复制到 cache mount 外的 `/app/rss-ai-news` 并 strip，供后续 stage 复制。

### 3.2 `debug` stage

- 基于 runtime 增加 bash、curl、sqlite3、jq 等诊断工具与 tini。
- 最终仍以非 root 的 `appuser` 运行，不单独发布 GHCR debug tag。

### 3.3 `runtime` stage

- `debian:bookworm-slim` 基础，按 index digest 固定（Debian 12 LTS 至 2028-06-30，暂不换发行版）
- 安装最小运行时依赖（`ca-certificates` 等）
- 从 builder 的 `/app/rss-ai-news` 复制最终二进制到 `/usr/local/bin/`。
- `ENTRYPOINT ["/usr/local/bin/rss-ai-news"]`，以 UID/GID 10001 的 `appuser` 运行。

### 3.4 `scheduler` stage

- `FROM runtime`
- 额外安装 supercronic 0.2.49（aptible/supercronic），按 amd64/arm64 校验固定 SHA-256。
- `COPY docker/scheduler-entrypoint.sh /usr/local/bin/`
- `ENTRYPOINT ["scheduler-entrypoint.sh"]`

### 3.5 Rust 工具链

唯一真相源是 `rust-toolchain.toml` 的 `channel`（当前 1.97.0）。同一版本另写在 `.github/workflows/` 的
`ci.yml`、`release.yml`（`RUST_TOOLCHAIN`）、`audit.yml`、`maintenance.yml`（×2，`toolchain:`）与
`docker/Dockerfile`（`FROM rust:<版本>-slim-bookworm@sha256:…`）共 6 处。升级时一次改全部；static acceptance lane 的
Rust builder 镜像的 patch tag 在官方支持列表里只停留到下一个 Rust 版本发布，之后不再重建，所以它的 digest 随工具链升级一起
刷新（Dependabot 只持续刷新 Debian 运行时镜像，见 [../operations/ci-and-release.md](../operations/ci-and-release.md)）。
`toolchain-pins` 检查遍历 `.github/workflows/` 下全部 `*.yml` / `*.yaml`（不是固定清单）：每个 `RUST_TOOLCHAIN:` 必须是该
literal，每个 `toolchain:` 输入只能是同一 literal 或恰好 `${{ env.RUST_TOOLCHAIN }}`（且同文件定义了它），其他表达式一律失败
（可能解析成别的编译器，R3-9）；每个 `dtolnay/rust-toolchain` 步骤对应一个 `toolchain:` 输入；Dockerfile 恰有一个
`FROM rust:` 且版本等于 channel。任一处漂移即失败。

## 4. scheduler 入口

[`docker/scheduler-entrypoint.sh`](../../docker/scheduler-entrypoint.sh) 支持两种调度形态：

### 4.1 多行 crontab 文件（推荐）

`RSS_CRONTAB_FILE`（默认 `/app/crontab`）挂载一个 supercronic 兼容的 crontab：

```cron
0 */3 * * *  sh -c '/usr/local/bin/rss-ai-news --config-dir /app/configs ingest && /usr/local/bin/rss-ai-news --config-dir /app/configs ai-run'
30 21 * * *  /usr/local/bin/rss-ai-news --config-dir /app/configs publish
```

文件是非空普通文件 → 直接交给 supercronic，跳过 env 模式；不存在或为空 → env 模式。
路径存在但不是普通文件（bind-mount 的宿主文件不存在时 Docker 会建出目录）→ entrypoint 报错退出。
适合需要分离 ingest 节奏与 publish 节奏的场景。

### 4.2 单行 env 模式（向后兼容）

```env
RSS_CRON_SCHEDULE="0 */6 * * *"
RSS_CRON_COMMAND="run --max-batches 3"
```

entrypoint 拼一行 `<RSS_CRON_SCHEDULE> /usr/local/bin/rss-ai-news --config-dir /app/configs <RSS_CRON_COMMAND>`
给 supercronic。supercronic 用 `/bin/sh -c` 执行命令部分，所以 `RSS_CRON_COMMAND` 按 shell 命令行书写、只被解析一次
（含空格的参数照常加引号）；两个变量含换行或回车时 entrypoint 拒绝启动。适合简单场景。

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

## 6. CI workflow

[`.github/workflows/ci.yml`](../../.github/workflows/ci.yml) 五 job 并行（无 `needs:` 链）：

| Job | 验证项 |
|---|---|
| `test` | acceptance `static` + `workspace` lanes：fmt / clippy / swallowed-error / dependency-security gates + `cargo test --workspace`（SQLite path） |
| `test-pg` | acceptance `postgres` + `sqlite` + `release` lanes：PostgreSQL service + PG-only integration / migration checks；SQLite migrate / CLI contract smoke；version identity |
| `docker-build` | `docker buildx build` 构建（不 push）+ 容器 smoke，验证 Dockerfile 不烂 |

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
| 分类配置模板（按需自建服务端配置） | [`configs/categories/ai.toml.example`](../../configs/categories/ai.toml.example) |
| PostgreSQL 迁移 | [`migrations/postgres/`](../../migrations/postgres/) |

代码路径过时时在 [../map/architecture-diff.md](../map/architecture-diff.md) 登记漂移。
