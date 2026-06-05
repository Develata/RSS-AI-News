# 本地开发

## 前置

| 工具 | 版本 | 备注 |
|---|---|---|
| Rust toolchain | `1.94.0`（与 `.github/workflows/ci.yml` 的 `RUST_TOOLCHAIN` 一致） | `rustup install 1.94.0` |
| cargo | 随 toolchain | |
| SQLite | 任意现代版 | sqlx 携带 bundled 驱动可省略本地装 |
| PostgreSQL（可选） | `pg16` 与 CI 一致 | 本地纯 SQLite 开发不需要 |

## 首次签出

```bash
git clone https://github.com/Develata/RSS-AI-News.git
cd RSS-AI-News
git config core.hooksPath .githooks   # 启用 pre-commit hook（v0.3.0 引入）
cargo build --workspace
```

## 日常循环

```bash
# 改完代码先 fmt（PostToolUse hook 自动跑，但手动也行）
cargo fmt --all

# 全量 clippy（CI 硬门槛）
cargo clippy --workspace --all-targets -- -D warnings

# 全量测试（SQLite path）
cargo test --workspace

# 只跑某 crate
cargo test -p rss-ai-news-runtime

# 只跑某测试
cargo test --workspace --test ingest_tests single_source_200_inserts_all_entries
```

## 子命令冒烟

```bash
# 1. 复制 example 配置
cp -r configs/categories.example configs/categories
cp configs/app.toml.example configs/app.toml
# 编辑 .env，注入 OPENAI_API_KEY / GITHUB_TOKEN / RSSHUB_BASE_URL

# 2. 校验配置
cargo run -- validate-config

# 3. 健康检查（轻量）
cargo run -- doctor

# 4. 初始化 schema
cargo run -- migrate run

# 5. 跑一次 ingest（不调 AI、不发布）
cargo run -- ingest --batch-size 10

# 6. 全链路一次
cargo run -- run --max-batches 1
```

## PostgreSQL 切换（本地）

```bash
# 1. 启动 PG（docker compose 或本机）
docker run -d --name rss-pg -e POSTGRES_PASSWORD=dev -p 5432:5432 postgres:16

# 2. 改 .env
echo 'DATABASE_URL=postgres://postgres:dev@127.0.0.1:5432/postgres' >> .env

# 3. 改 app.toml
# [database]
# driver = "postgres"

# 4. 重新 migrate
cargo run -- migrate run
```

详见 [./postgres-deployment.md](./postgres-deployment.md)。

## pre-commit hook

`.githooks/pre-commit` 在 v0.3.0 引入。启用方式：`git config core.hooksPath .githooks`。
功能：
- `cargo fmt --check`：未 fmt 拒绝 commit
- `cargo clippy --workspace -- -D warnings`：warning 拒绝 commit

> hook 是用户**主动**启用的，不在 `git clone` 后自动接管。

## codegraph 索引（可选 agent 工作流）

```bash
# 首次构建索引
codegraph init -i

# 索引位于 .codegraph/codegraph.db（已在 .gitignore，不入仓）
```

agent 协作时建议刷新索引。详见 [../AGENTS.md](../AGENTS.md)。

## 常见踩坑

| 现象 | 原因 | 解决 |
|---|---|---|
| `error: failed to run custom build command for openssl-sys` | OpenSSL dev headers 缺失（Linux） | `apt install pkg-config libssl-dev` 或切 rustls |
| `cargo test` PG 测试全跳过 | `DATABASE_URL` 未设 | 设 env 或忽略（test crate 自检后 skip） |
| `Address already in use (os error 98)` —— metrics-bind | 9090 端口被占 | 改 `--metrics-bind 127.0.0.1:9091` |
| `pre-commit hook 不生效` | `core.hooksPath` 未设置 | `git config core.hooksPath .githooks` |
| `cargo fmt` Windows 改行符警告 | autocrlf 默认 true | 仓库内 `.editorconfig` 强制 LF；忽略警告或 `git config core.autocrlf input` |

## 相关文档

- 部署（生产）：[./docker-and-scheduler.md](./docker-and-scheduler.md)
- CI / Release：[./ci-and-release.md](./ci-and-release.md)
- 排障：[./troubleshooting.md](./troubleshooting.md)
- 设计：[../plan/12-deployment.md](../plan/12-deployment.md)
