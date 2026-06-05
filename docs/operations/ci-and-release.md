# CI 与 Release 流程

## CI workflow

文件：[`.github/workflows/ci.yml`](../../.github/workflows/ci.yml)。

触发：push 到 `main` 或 PR target `main`。

```text
concurrency: ci-${{ workflow }}-${{ ref }}   # 同分支后续 push 自动取消前一次
```

四个并行 job（无 `needs:` 依赖链，独立 toolchain + cache）：

| Job | 验证项 | 失败处理 |
|---|---|---|
| `lint` | `cargo fmt --check` + `cargo clippy --workspace -- -D warnings` | 本地跑 `cargo fmt --all` + 清理 warning |
| `test` | `cargo test --workspace`（SQLite） | 本地 `cargo test -p <crate>` 复现 |
| `migrate` | postgres service + `migrate run` 端到端 + 部分 PG 集成测试 | 本地启 PG 复现（见 [./postgres-deployment.md](./postgres-deployment.md)） |
| `docker-build` | `docker buildx build` 多 stage 构建（不 push） | 本地 `docker build -f docker/Dockerfile .` 复现 |

CI 通过是 PR 合入 `main` 的硬门槛。文档变更也不豁免（hooks 触发 `cargo fmt`）。

## Release workflow

文件：[`.github/workflows/release.yml`](../../.github/workflows/release.yml)。

触发：push tag `v*`（如 `v0.4.0` / `v0.4.0-rc.1`）。**不**触发于 branch push / PR。

```yaml
permissions:
  contents: read
  packages: write   # 推 GHCR 需要
env:
  RUST_TOOLCHAIN: "1.94.0"     # 与 ci.yml 保持一致
  IMAGE_NAME: ghcr.io/${{ github.repository }}   # 自动 lowercase
```

Job：`publish-image` —— 同时构建发布 runtime + scheduler 两套镜像。

## 镜像 tag 规则

由 `docker/metadata-action` 计算：

| 推送 tag | 生成镜像 tag |
|---|---|
| `v0.4.0` | `0.4.0`、`0.4`、`0`、`latest` |
| `v0.4.0-rc.1` | `0.4.0-rc.1`（仅完整版；**不**打 `latest`） |
| `v1.0.0` | `1.0.0`、`1.0`、`1`、`latest` |

`develata` lowercase 后即 GHCR 命名空间（owner）。

## 发版流程

```bash
# 1. 完成本轮所有 feature commit、CI 全绿、code review 通过
git checkout main && git pull

# 2. 决定版本号（参照 docs/reports/releases/*.md 的范围划分；目前仅 minor 走 release report）
#    bug fix → vX.Y.Z+1   feature → vX.Y+1.0   breaking → vX+1.0.0

# 3. 打 tag（带说明）
git tag -a v0.5.0 -m "Release v0.5.0: <一句话>"

# 4. push
git push origin v0.5.0

# 5. 等 release workflow 跑完（5-15 分钟），确认 GHCR 镜像出现
gh release view v0.5.0 || \
docker pull ghcr.io/develata/rss-ai-news:0.5.0

# 6. 写 release report
$EDITOR docs/reports/releases/v0.5.0.md
git add docs/reports/releases/v0.5.0.md
git commit -m "docs(reports): v0.5.0 release notes"
git push origin main
```

## 镜像版本提升

部署侧：

```bash
# 拉新版
docker pull ghcr.io/develata/rss-ai-news:0.5.0

# 若有 schema 变化，先 migrate（不动 scheduler 容器）
docker run --rm --env-file .env \
  -v "$(pwd)/configs:/app/configs:ro" \
  -v "$(pwd)/data:/app/data" \
  ghcr.io/develata/rss-ai-news:0.5.0 \
  --config-dir /app/configs migrate run

# 升级 scheduler
docker compose -f docker-compose.scheduler.yml pull
docker compose -f docker-compose.scheduler.yml up -d --force-recreate
```

## 已知发布事件

| Tag | 日期 | report |
|---|---|---|
| v0.1.0 | 2026-05-22 | [../reports/releases/v0.1.0.md](../reports/releases/v0.1.0.md) |
| v0.1.1 | 2026-05-20 | patch（未单独 report） |
| v0.1.2 | 2026-05-21 | patch（未单独 report） |
| v0.2.0 | 2026-05-22 | [../reports/releases/v0.2.0.md](../reports/releases/v0.2.0.md) |
| v0.3.0 | 2026-05-22 | [../reports/releases/v0.3.0.md](../reports/releases/v0.3.0.md) |
| v0.3.1 | 2026-05-25 | patch（未单独 report） |
| v0.4.0 | 2026-05-25 | [../reports/releases/v0.4.0.md](../reports/releases/v0.4.0.md) |

> 当前约定：minor / major release 写 report；patch（修 bug、不影响 surface）不写。

## 相关文档

- workflow 文件：[`.github/workflows/ci.yml`](../../.github/workflows/ci.yml)、[`.github/workflows/release.yml`](../../.github/workflows/release.yml)
- 部署：[./docker-and-scheduler.md](./docker-and-scheduler.md)
- 设计：[../plan/12-deployment.md](../plan/12-deployment.md)
