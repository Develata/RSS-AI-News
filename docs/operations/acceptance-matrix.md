# Rust 验收矩阵 CLI

## 目的

`rss-ai-news-acceptance` 是仓库级、Rust-native 的验收编排器。它把分散的 Cargo、CLI、SQLite、PostgreSQL、Docker 与 release identity checks 组织为可枚举、可单跑、可生成 JSON evidence 的 lanes；不复制业务实现，也不进入 production binary。

## 架构边界

- 位置：`tools/acceptance/`，作为 workspace tooling crate。
- 依赖方向：只依赖通用 CLI/serialization crates；**不得依赖** `rss-ai-news-*` product crates。
- product crates、runtime image 与 scheduler image 不依赖该工具。
- runner 只通过稳定边界验收系统：Cargo commands、`.ci` policy scripts、编译后的 `rss-ai-news` CLI、PostgreSQL URL 与 Docker CLI。
- 业务 truth 仍由 product tests、acceptance cases 与 CI workflows 持有；runner 负责 orchestration 和 evidence，不重写业务断言。

这保证高内聚低耦合：验收编排集中在一个 tooling crate，业务层不感知验收基础设施；任一 lane 可被替换而不改写 product flow。

## CLI contract

```bash
# 枚举 lanes / profiles
cargo acceptance list
cargo acceptance --format json list

# 零副作用地查看计划
cargo acceptance run --profile local --dry-run
cargo acceptance run --profile full --dry-run

# 本机可复现的 pre-tag matrix
cargo acceptance run --profile local --expected-version 0.7.1

# 完整 matrix；缺 Docker / DATABASE_URL / PostgreSQL 时必须失败
DATABASE_URL='postgres://...' \
  cargo acceptance run --profile full --expected-version 0.7.1

# 单 lane，适合 CI 或故障复现
cargo acceptance run --lane static
cargo acceptance run --lane workspace
cargo acceptance run --lane sqlite --expected-version 0.7.1
cargo acceptance run --lane postgres
cargo acceptance run --lane docker
cargo acceptance run --lane release --expected-version 0.7.1
```

`--lane` 可重复；显式 lanes 与 `--profile` 互斥。默认 profile 为 `local`。默认收集全部选中 lane 的结果；`--fail-fast` 可在首个失败后停止。

## Profiles 与 lanes

| Lane | local | full | 责任 |
|---|---:|---:|---|
| `static` | ✓ | ✓ | rustfmt、Clippy、swallowed-error、dependency policy |
| `workspace` | ✓ | ✓ | workspace locked build/test |
| `sqlite` | ✓ | ✓ | release binary、SQLite migrate run/check、CLI contract smokes |
| `postgres` |  | ✓ | PG ignored tests与 CLI migrate parity；要求 `DATABASE_URL` + Docker |
| `docker` |  | ✓ | runtime/debug/scheduler build 与 container smokes |
| `release` | ✓ | ✓ | workspace/lock/binary/README/version identity 与 Git diff check |

## `recent-entries` opt-in gate

SQLite lane 必须用真实 release binary 验证两种调用：

1. 省略 `--published-after`：成功 JSON 中 `summary.published_after == null`；
2. 显式传入 RFC3339 cutoff：成功 JSON 回显同一 instant。

因此 publication-time filtering 是 consumer opt-in；仅存在该 CLI 参数不会改变默认查询。

## 输出与失败语义

- `pretty`：输出 lane/step 状态与失败日志 tail。
- `json`：stdout 只输出一个 schema-versioned report；child output 被有界截断后放入 step evidence，不污染 JSON envelope。
- failure evidence 在截断前按 explicit child env secret、敏感 inherited parent env snapshot、URL userinfo、敏感 key assignment 与 Bearer token 做独立 redaction；tooling 不依赖 product observability crate。
- 所有选中 lanes passed：exit `0`。
- 任一 lane failed、prerequisite 缺失或 contract mismatch：exit `1`。
- CLI 参数错误：Clap exit `2`。
- `--dry-run` 不执行 Cargo、Docker、product CLI，不创建 smoke workspace 或数据库。
- `--fail-fast` 在首个失败后不创建后续 smoke config/database；已经创建的 exact-name Docker resources 仍无条件 cleanup。
- child Cargo 默认使用 `CARGO_BUILD_JOBS=1`、`CARGO_INCREMENTAL=0`、`CARGO_PROFILE_DEV_DEBUG=0`，降低小 volume 上并行 linker 与 incremental artifacts 的资源峰值；调用方已显式设置同名变量时不覆盖。

## Deterministic boundary

该矩阵不拿真实 feed、AI provider 或 GitHub publish credentials 做 tag gate：这些表面具有外部波动和真实写入副作用。它们由 mock/integration tests、部署侧 smoke 与 release image read-back分别验证。`full` 表示仓库可自动、可重复的完整功能矩阵，不表示对第三方服务做生产写入。

## CI 与 release

CI 仍以 `.github/workflows/ci.yml` 的五个隔离 jobs 为并行执行真相源；每个 job 与上述 lane 一一对应或覆盖其超集。Tag 前必须：

1. 在 candidate commit 运行 `local` profile；
2. push candidate commit，确认 exact-SHA 五个 CI jobs 全绿（由 CI 执行 PG/Docker lanes）；
3. 运行 `release` lane，确认 `--version`、workspace version、Cargo.lock 与 README 均为候选版本；
4. 再创建 annotated stable tag。
