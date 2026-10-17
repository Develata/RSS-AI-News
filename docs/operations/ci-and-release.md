# CI 与 Release 流程

## CI workflow

文件：[`.github/workflows/ci.yml`](../../.github/workflows/ci.yml)。

触发：push 到 `main` 或 PR target `main`。

```text
concurrency: ci-${{ workflow }}-${{ ref }}   # 同分支后续 push 自动取消前一次
```

三个并行 job（无 `needs:` 依赖链）。job 按共享的编译产物分组：前两个调用 Rust 验收矩阵的 lane，
CI 与本地 pre-tag 矩阵执行同一份检查（见 [./acceptance-matrix.md](./acceptance-matrix.md)）。runner 不 fail-fast，
同一 job 内前一个 lane 失败后后续 lane 仍执行并报告：

| Job | 命令 / 验证项 | 本地复现 |
|---|---|---|
| `test` | `cargo acceptance run --lane static --lane workspace`：fmt、Clippy `-D warnings`、swallowed-error、dependency policy、tooling boundary、acceptance-case 状态；`cargo build/test --workspace --locked`（SQLite） | 同命令，或 `cargo test -p <crate>` |
| `test-pg` | `cargo acceptance run --lane postgres --lane sqlite --lane release`（PG service + `DATABASE_URL`）：storage `--include-ignored` + CLI migrate run/check；三条 lane 共用一次 release build：SQLite migrate run/check、recent-entries 与 CLI JSON 输出契约（product smoke 会剥离继承的 `DATABASE_URL`，仍走 SQLite）；版本 / lockfile / README / binary identity | `DATABASE_URL=… cargo acceptance run --lane postgres --lane sqlite --lane release`（见 [./postgres-deployment.md](./postgres-deployment.md)） |
| `docker-build` | `docker/build-push-action` 多 stage 构建（不 push）+ 容器 smoke；保留该形式以维护 release 复用的 GHA buildx cache（`scope=runtime`） | `cargo acceptance run --lane docker`，或 `docker build -f docker/Dockerfile .` |

缓存：

- Rust job 用 `Swatinem/rust-cache`（`shared-key` = `test` / `test-pg`），只有 `main` 保存缓存（PR ref 下的缓存 `main`
  读不到）。release `verify-*` 只读这些 key，因此 release.yml 的 `CARGO_*` / `RUST_*` env 必须与 ci.yml 一致（它们进 key 的
  environment hash）。environment hash 保持开启：rust-cache v2 关闭它会连 Cargo.lock hash 一起去掉，key 永不变化、缓存
  永不刷新。代价是 runner 镜像升级其预装 stable Rust 时（如 1.98.1 → 1.99.0），全部 Rust 缓存失效一次。
- Docker builder 用 cargo-chef：依赖编译是按 manifest-only recipe（Cargo.lock + manifests，workspace 成员版本被屏蔽）
  缓存的普通 layer，经 `cache-to: type=gha` 持久化（与 rust-cache 一致，只有 `main` 写入；PR 只读，release 的 tag
  run 只读 `main` 的 `scope=runtime`）；源码改动只重编 workspace crates。`RUN --mount=type=cache`
  的内容不会被 `type=gha` 导出，所以不用它承载依赖缓存。
- GitHub 会驱逐 7 天未访问的缓存；停更超过一周后的首个 run 必然冷启动（约多 10–15 runner-min），这是预期行为。

历史（2026-05-03 → 2026-10-16，109 runs / 1006 runner-min）：`test-pg` 在 55/70 次 CI 中是关键路径；2026-09-10
Dockerfile 改用 cache mount 后，改代码的 docker job 中位数由 2.2 min 升到 5.3 min（依赖全量重编）；上述 cargo-chef layer 用于消除这一回退（本地已验证，CI 实测待首次 warm run 确认）。
旧基线（2026-09-11 前 5 次运行中位数，五 job 布局）：整次 CI 326s，`test-pg` ≈282s，其中 storage PG 测试 181s、
release build 52s。PG 测试保持 `--test-threads=1`：本地实测 1 线程 108s、4 线程 99s，收益不足以承担连接争用风险。

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

- `verify-test` —— `cargo acceptance run --lane static --lane workspace`；
- `verify-release` —— `cargo acceptance run --lane sqlite --lane release --expected-version "${GITHUB_REF_NAME#v}"`：tag 版本必须与
  workspace / lockfile / README / binary 一致。两者合起来即 `local` profile 的四条 lane，在 tag commit 上通过；
  它们只读 ci.yml 在 `main` 上为同一 commit 保存的缓存（`test-*` / `test-pg-*`），未命中只是变慢。
- `publish-image`（`needs: [verify-test, verify-release]`）—— 同时构建发布 runtime + scheduler 两套镜像；任一 verify 失败则不推镜像。

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
#    并确认 main exact SHA 的三个 CI jobs 全绿

git checkout main && git pull

# 2. 决定版本号：bug fix → vX.Y.Z+1；feature → vX.Y+1.0；breaking → vX+1.0.0
#    本项目只发 stable tag，不创建 prerelease tag；release.yml 的 verify gate 只接受 X.Y.Z，
#    推送 prerelease tag 会在 verify 失败、不推镜像（下方 rc 行仅说明 metadata-action 的映射规则）

# 3. 在 tag 前写好 release report，使 release notes 随 tag 一起冻结
$EDITOR docs/reports/releases/v0.7.0.md
git add docs/reports/releases/v0.7.0.md
git commit -m "docs(reports): prepare v0.7.0 release notes"
git push origin main
# 等上述 exact SHA CI 全绿

# 4. 打 annotated tag（带说明）
git tag -a v0.7.0 -m "Release v0.7.0: <一句话>"

# 5. push tag（release workflow 先跑 verify-*；通过后才写 GHCR。在 exact-SHA CI 完成后再推，
#    verify 才能复用 CI 刚保存的缓存）
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
| v0.9.0 | 2026-09-26 | [../reports/releases/v0.9.0.md](../reports/releases/v0.9.0.md) |
| v0.9.1 | 2026-10-16 | [../reports/releases/v0.9.1.md](../reports/releases/v0.9.1.md) |

> 当前约定：minor / major release 写 report；纯 bugfix patch 可不写。改变公开 surface 或 release tooling 的 patch 仍写 report。

## 相关文档

- workflow 文件：[`.github/workflows/ci.yml`](../../.github/workflows/ci.yml)、[`.github/workflows/release.yml`](../../.github/workflows/release.yml)
- 部署：[./docker-and-scheduler.md](./docker-and-scheduler.md)
- 设计：[../plan/12-deployment.md](../plan/12-deployment.md)
