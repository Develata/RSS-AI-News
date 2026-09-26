# CI 与 Release 流程

## CI workflow

文件：[`.github/workflows/ci.yml`](../../.github/workflows/ci.yml)。

触发：push 到 `main` 或 PR target `main`。

```text
concurrency: ci-${{ workflow }}-${{ ref }}   # 同分支后续 push 自动取消前一次
```

五个并行 job（无 `needs:` 依赖链，独立 toolchain + cache）。前四个直接调用 Rust 验收矩阵的 lane，
CI 与本地 pre-tag 矩阵执行同一份检查（见 [./acceptance-matrix.md](./acceptance-matrix.md)）：

| Job | 命令 / 验证项 | 本地复现 |
|---|---|---|
| `lint` | `cargo acceptance run --lane static`：fmt、Clippy `-D warnings`、swallowed-error、dependency policy、tooling boundary、acceptance-case 状态 | 同命令 |
| `test` | `cargo acceptance run --lane workspace`：`cargo build/test --workspace --locked`（SQLite） | 同命令，或 `cargo test -p <crate>` |
| `migration-smoke` | `cargo acceptance run --lane sqlite --lane release`：共用一次 release build；SQLite migrate run/check、recent-entries 与 CLI JSON 输出契约；版本 / lockfile / README / binary identity | 同命令 |
| `test-pg` | `cargo acceptance run --lane postgres`（PG service + `DATABASE_URL`）：storage `--include-ignored` + CLI migrate run/check | `DATABASE_URL=… cargo acceptance run --lane postgres`（见 [./postgres-deployment.md](./postgres-deployment.md)） |
| `docker-build` | `docker/build-push-action` 多 stage 构建（不 push）+ 容器 smoke；保留该形式以维护 release 复用的 GHA buildx cache（`scope=runtime`） | 本地 `docker build -f docker/Dockerfile .` |

基线（2026-09-11 前 5 次运行中位数）：整次 CI 326s，关键路径为 `test-pg`（≈282s，其中 storage PG 测试
181s、release build 52s）或 docker cache miss（≈300s）。PG 测试保持 `--test-threads=1`：本地实测
1 线程 108s、4 线程 99s，收益不足以承担连接争用风险。

CI 通过是 PR 合入 `main` 的硬门槛。文档变更也不豁免（hooks 触发 `cargo fmt`）。

## Release workflow

文件：[`.github/workflows/release.yml`](../../.github/workflows/release.yml)。

触发：push tag `v*`（如 `v0.4.0` / `v0.4.0-rc.1`）。**不**触发于 branch push / PR。

```yaml
permissions:
  contents: read
  packages: write   # 推 GHCR 需要
env:
  RUST_TOOLCHAIN: "1.97.0"     # 与 ci.yml 保持一致
  IMAGE_NAME: ghcr.io/${{ github.repository }}   # 自动 lowercase
```

Jobs：

- `verify` —— `cargo acceptance run --profile local --expected-version "${GITHUB_REF_NAME#v}"`：tag 版本必须与
  workspace / lockfile / README / binary 一致，且 credential-free 矩阵在 tag commit 上通过。
- `publish-image`（`needs: verify`）—— 同时构建发布 runtime + scheduler 两套镜像；`verify` 失败则不推镜像。

## 镜像 tag 规则

由 `docker/metadata-action` 计算：

| 推送 tag | 生成镜像 tag |
|---|---|
| `v0.7.0` | `0.7.0`、`0.7`、`latest` |
| `v0.7.0-rc.1` | `0.7.0-rc.1`（仅完整版；**不**打 `latest`） |
| `v1.0.0` | `1.0.0`、`1.0`、`latest` |

`develata` lowercase 后即 GHCR 命名空间（owner）。runtime 使用上述 tag；scheduler 位于**同一 package**，
对应 `<tag>-scheduler`（例如 `ghcr.io/develata/rss-ai-news:0.7.0-scheduler`）。workflow 不生成
major-only alias（如 `0` / `1`）。

## 发版流程

```bash
# 1. 完成 feature/fix、将 workspace package version 对齐候选 tag、跑完 security scan/review
#    并确认 main exact SHA 的五个 CI jobs 全绿

git checkout main && git pull

# 2. 决定版本号：bug fix → vX.Y.Z+1；feature → vX.Y+1.0；breaking → vX+1.0.0
#    本项目只发 stable tag，不创建 prerelease tag

# 3. 在 tag 前写好 release report，使 release notes 随 tag 一起冻结
$EDITOR docs/reports/releases/v0.7.0.md
git add docs/reports/releases/v0.7.0.md
git commit -m "docs(reports): prepare v0.7.0 release notes"
git push origin main
# 等上述 exact SHA CI 全绿

# 4. 打 annotated tag（带说明）
git tag -a v0.7.0 -m "Release v0.7.0: <一句话>"

# 5. push tag（release workflow 先跑 verify；通过后才写 GHCR，不等待 branch CI）
git push origin v0.7.0

# 6. 等 release workflow 完成
run_id=$(gh run list --workflow release.yml --branch v0.7.0 --json databaseId --jq '.[0].databaseId')
gh run watch "$run_id" --exit-status

# 7. 校验两个 immutable image tags，并执行 pull / --version / --help smoke
docker manifest inspect ghcr.io/develata/rss-ai-news:0.7.0
docker manifest inspect ghcr.io/develata/rss-ai-news:0.7.0-scheduler
docker pull ghcr.io/develata/rss-ai-news:0.7.0
docker run --rm ghcr.io/develata/rss-ai-news:0.7.0 --version
docker run --rm ghcr.io/develata/rss-ai-news:0.7.0 --help
docker pull ghcr.io/develata/rss-ai-news:0.7.0-scheduler
docker run --rm --entrypoint /usr/local/bin/rss-ai-news \
  ghcr.io/develata/rss-ai-news:0.7.0-scheduler --version
```

workflow 只发布 GHCR images，不创建 GitHub Release。release 完成后把 run id、manifest digest 与 smoke
结果回填到 report，作为 post-release evidence。

## 镜像版本提升

部署侧：

```bash
# 拉新版
docker pull ghcr.io/develata/rss-ai-news:0.7.0

# 若有 schema 变化，先 migrate（不动 scheduler 容器）
docker run --rm --env-file .env \
  -v "$(pwd)/configs:/app/configs:ro" \
  -v "$(pwd)/data:/app/data" \
  ghcr.io/develata/rss-ai-news:0.7.0 \
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
| v0.5.0 | 2026-06-12 | [../reports/releases/v0.5.0.md](../reports/releases/v0.5.0.md) |
| v0.6.0 | 2026-06-17 | [../reports/releases/v0.6.0.md](../reports/releases/v0.6.0.md) |
| v0.6.1 | 2026-06-19 | patch（未单独 report） |
| v0.6.2 | 2026-06-19 | patch（未单独 report） |
| v0.7.0 | 2026-07-17 | [../reports/releases/v0.7.0.md](../reports/releases/v0.7.0.md) |
| v0.7.1 | 2026-07-18 | [../reports/releases/v0.7.1.md](../reports/releases/v0.7.1.md) |
| v0.8.0 | 2026-09-10 | [../reports/releases/v0.8.0.md](../reports/releases/v0.8.0.md) |
| v0.8.1 | 2026-09-11 | [../reports/releases/v0.8.1.md](../reports/releases/v0.8.1.md) |

> 当前约定：minor / major release 写 report；纯 bugfix patch 可不写。改变公开 surface 或 release tooling 的 patch 仍写 report。

## 相关文档

- workflow 文件：[`.github/workflows/ci.yml`](../../.github/workflows/ci.yml)、[`.github/workflows/release.yml`](../../.github/workflows/release.yml)
- 部署：[./docker-and-scheduler.md](./docker-and-scheduler.md)
- 设计：[../plan/12-deployment.md](../plan/12-deployment.md)
