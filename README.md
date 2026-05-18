# RSS-AI-News

Rust 重构版的 RSS-AI-News：单进程一次性 CLI，按调度外部触发，串起
**抓取 → 正文提取 → AI 摘要/分类 → 报告生成 → 发布** 的端到端管线。

- 12 crate workspace，edition 2024 / resolver 3，构建工具链下限 **Rust 1.88+**（受 Cargo.lock 中 `time@0.3.47` / `icu_*@2.2.0` 等传递依赖约束）
- **双 backend 存储**：SQLite（默认）/ PostgreSQL（W11 起实装），按 `database.driver` 与 `DATABASE_URL` 路由（详见 [`docs/design/storage-multi-dialect.md`](./docs/design/storage-multi-dialect.md)）。CI 双轨覆盖 PG 路径
- rustls-only TLS，无 OpenSSL native
- 单进程 / 单次执行，**无内置 cron**：调度交给外部（cron / systemd timer / GitHub Actions / k8s CronJob）
- 状态机驱动 + artifact 留痕，所有"对外副作用"均可幂等回放

---

## 快速开始

### 路径 A：Docker（推荐）

```bash
# 1. 准备配置
cp .env.example .env
$EDITOR .env                                 # 至少填 OPENAI_API_KEY；用 RSSHub 模板的 source 还需 RSSHUB_BASE_URL
cp configs/app.toml.example configs/app.toml
cp configs/categories/ai.toml.example configs/categories/ai.toml

# 2. 构建运行时镜像（首次约 5–8 分钟，含完整 cargo build）
docker build -f docker/Dockerfile --target runtime -t rss-ai-news:runtime .

# 3. 校验配置
docker run --rm --env-file .env \
  -v "$PWD/configs:/app/configs:ro" \
  rss-ai-news:runtime --config-dir /app/configs validate-config

# 4. 跑一次完整管线（dry-run 由各子命令自己决定是否支持）
docker run --rm --env-file .env \
  -v "$PWD/configs:/app/configs:ro" \
  -v "$PWD/data:/app/data" \
  rss-ai-news:runtime --config-dir /app/configs run
```

或用 docker compose（profile 化）：

```bash
docker compose --profile runtime -f docker/docker-compose.yml up rss-ai-news
docker compose --profile debug   -f docker/docker-compose.yml up rss-ai-news-debug   # bash/curl/sqlite3/jq 等诊断工具
```

### 路径 B：本地 cargo

```bash
# 一次性
cargo build --release --bin rss-ai-news
./target/release/rss-ai-news --config-dir configs validate-config

# 或开发态
cargo run --bin rss-ai-news -- --config-dir configs validate-config
```

需要本地 Rust 1.88+（同上工具链下限说明）。

---

## 必填环境变量

| 变量 | 何时必填 |
|---|---|
| `OPENAI_API_KEY`   | `[ai].enabled = true` 时（默认开启） |
| `OPENAI_BASE_URL`  | 同上；默认 `https://api.openai.com/v1` |
| `RSSHUB_BASE_URL`  | 任一 source 使用 `{RSSHUB}` 占位符时 |
| `GITHUB_TOKEN`     | publish 远端模式且 `publish.github_owner` 非空时 |
| `DATABASE_URL`     | `database.driver = "postgres"` 时**必填**（`postgres://` 或 `postgresql://` scheme）；`driver = "sqlite"` 时可空，留空则用 `database.sqlite_path` |
| `HTTP_PROXY` / `HTTPS_PROXY` | 可选；进程级代理 |

校验未通过时进程以 **exit 78**（`ConfigError`）退出，错误信息会逐项指出缺哪个变量。

---

## 切换到 PostgreSQL（W11+）

`storage` crate 自 W11 起双 backend 实装，11 个 repo 全部双轨化。

```toml
# configs/app.toml
[database]
driver = "postgres"
# sqlite_path 在 driver = "postgres" 时被忽略，但字段仍需存在（schema 校验）
sqlite_path = "data/rss-ai-news.db"
max_connections = 8
busy_timeout_ms = 5000  # PG 路径忽略
```

```bash
# .env
DATABASE_URL=postgres://user:pass@host:5432/dbname
```

启动期 `cli::context_factory` 会校验 `driver` 与 `DATABASE_URL` scheme
一致（见 [`docs/design/storage-multi-dialect.md`](./docs/design/storage-multi-dialect.md) §5.4），
不一致即 exit 78。

**初始化 PG 库**：

```bash
DATABASE_URL=postgres://... rss-ai-news --config-dir configs migrate run
DATABASE_URL=postgres://... rss-ai-news --config-dir configs migrate check
```

migrations 按 backend 分目录（`migrations/sqlite/` vs `migrations/postgres/`），
但共享 schema 编号空间（CI 校验对偶）。

**已知边界（W11 P3 阶段）**：

- `cli run / ingest / ai-run / publish / doctor` 端到端 PG 路径**尚未接通**
  （cli/runtime 仍用 `Repo::new(SqlitePool)` 旧入口；W11 P4-C 待办）
- PG 路径目前能用：`cli migrate run/check` + storage crate 测试套件
- 跟踪：[`docs/design/storage-multi-dialect.md`](./docs/design/storage-multi-dialect.md) §9 P4 行

---

## 退出码

| Exit | 含义 |
|---|---|
| 0  | Success |
| 1  | RuntimeError（管线/IO/存储/远端调用失败） |
| 2  | UserError（参数误用） |
| 78 | ConfigError（配置 fail-fast 不通过） |

CI 脚本应据此判断成败。

---

## CLI 子命令

```
rss-ai-news [global flags] <command>
```

| 命令 | 用途 |
|---|---|
| `validate-config` | 加载并校验全部 config / .env，打印 SHA-256 |
| `migrate run`     | 应用所有未执行的迁移 |
| `migrate check`   | 检查 schema 与 migration 列表是否一致，不写库 |
| `ingest`          | 抓取 feed → 入 `articles`，按 `--batch-size` 分批 |
| `ai-run`          | 取未处理 article → 调 OpenAI → 写 `ai_results` |
| `publish`         | 取已生成报告 → 本地或 GitHub 发布 |
| `rebuild-report`  | 按现有数据重渲染 markdown 报告（不调 AI） |
| `reindex`         | 重建 `articles` 的全文索引 |
| `backfill`        | 历史日期范围内补跑 ingest + ai + report |
| `replay`          | 按 artifact id 回放某次 IO，幂等 |
| `doctor`          | 健康自检：配置、迁移、外部依赖、artifact 完整性 |
| `run`             | `ingest → ai-run → rebuild-report → publish` 的串联 |

全局 flag：`--config-dir` `--db-path` `--log-level` `--log-format {pretty,json}`
`--output-format {pretty,json}` `--dry-run` `--category` `--timezone`。

---

## 文档体系

按"宪法 → 设计哲学 → 工程蓝图 → 任务拆解"组织，全部位于 [`docs/`](./docs)：

- 入口：[docs/README.md](./docs/README.md)
- 工程宪法：[docs/constitution.md](./docs/constitution.md)
- 设计契约：[docs/design/](./docs/design)
- 实施蓝图：[docs/plan/full-rust-rss-ai-news-blueprint.md](./docs/plan/full-rust-rss-ai-news-blueprint.md)
- 任务分解：[docs/task/full-rust-rss-ai-news-blueprint-tasks.md](./docs/task/full-rust-rss-ai-news-blueprint-tasks.md)
- 工作日志（append-only）：[docs/handoffs/](./docs/handoffs)

---

## 开发

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
```

CI（GitHub Actions，见 [`.github/workflows/ci.yml`](./.github/workflows/ci.yml)）覆盖：
`lint` / `test` / `migration-smoke`（sqlite 上跑 `migrate run` + `migrate check`）/
`test-pg`（postgres:16-alpine service + storage `--include-ignored` 全套 +
`migrate run/check` on PG）/ `docker-build`（runtime + debug 镜像构建并
`--help` 冒烟）。

Windows 上单机内存紧张时用 `CARGO_BUILD_JOBS=1` 限制并发。

---

## 状态

W0–W11 P3 全部交付（详见任务文档）。storage 多方言双 backend 落地：
11 个 repo 全部 SQLite/PostgreSQL 双轨化，PG-only / 双轨 smoke 测试约
50 条全绿，CI `test-pg` 覆盖 PG 路径。W11 P4-C/D（cli/runtime 切换 +
全量集成测试参数化）进行中。首版 `0.1.0`。

License: MIT。
