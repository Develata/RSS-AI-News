# Artifact TTL 与配置真实性维护

- 日期：2026-09-10
- 作者：Codex
- 分支：main
- 基准 HEAD：`b3ec59856796f34610432e508b3637ace61c80b6`
- 相关 commit：`92423ba`（TTL）、`b746251`（配置）、`69c1722`（发版准备）
- 相关 release：v0.8.1，已发布并核验 stable / latest
- 状态：`validated`（已发布；本地、远端 CI、镜像与 Release 读回通过）

## 关键变更

- `RawArtifactRepository::purge_expired` 与 SQLite/PG 实现；旧自定义 repository 默认返回明确 unsupported 错误，保持 trait 实现的源码兼容。
- ingest / AI process 启动时各清理一批 500 条过期 inline artifact；失败 best-effort，非零发事件，零静默。
- 0005 在双方言增加两列 artifact 外键索引；既有 0001–0004 字节不变。FK SET NULL 保留文章、AI 结果和快照。
- validate-config 新增非致命、结构化 inert_config warning；15 个字段/枚举值的非默认设置可见，默认示例基线静默；TOML 日志值在 CLI override 前诊断。
- README 去掉 governor 保证；plan/06 列出全部保留字段，plan/10 统一 inline/debug_only/TTL 语义；地图与验收用例同步。
- 固定 v0.7.1 配置字节及哈希；旧 schema + 代表性文章/AI/冻结报告数据的升级与索引回退夹具。

## 验证

- 红→绿：ingest 原先遗留 501 条到期记录，修复后单次仅剩 1 条；原 validate-config 缺 warnings，修复后结构化/pretty 告警且仍成功。
- SQLite artifact repository、外键与 EXPLAIN、并发清理、旧 DB 升级/回退、旧配置夹具：通过。
- 原生 PostgreSQL 17.11，独立 Unix socket、临时 schema：升级/快照保留、并发 3+3 上限、NULL/未来/同秒/file 保留、SKIP LOCKED 保护续期、FK SET NULL、索引迁移回退均通过。
- `cargo test -p rss-ai-news-config -p rss-ai-news-storage -p rss-ai-news-runtime -p rss-ai-news-cli --locked`：648 passed / 0 failed / 64 ignored（含已存在的隔离子进程测试）；之后新增的清理失败与 PG 锁场景分别编译验证，runtime 专项 3 passed，SQLite artifact 专项 7 passed / PG 2 ignored。
- `cargo clippy --workspace --all-targets --jobs 1 -- -D warnings`：通过；fmt、swallowed-error（11 条既有豁免）、dependency security policy：通过。
- 原始日志：`affected-tests.log`、`runtime-final.log`、`storage-final.log`、`compatibility.log`、`config-cli.log`、`pg-probe.log`、`clippy.log`，均在上述证据目录。
- CLI JSON 仅在有 warning 时增加 `summary.warnings`；Rust 手工构造 `ValidateConfigSummary` 的调用方需补充 warnings 字段，现有 aggregate 字段不删除。
- 未运行 PostgreSQL 16 Docker fixture（本机无 Linux Docker daemon）；已为 CI `--include-ignored` 添加对应测试。本轮未使用生产数据库、凭据或目标仓库报告写入。

## 升级与边界

- 此维护候选新增 0005：升级需显式 migrate run/check；在大库创建两个索引会有一次性的扫描和锁影响，应安排维护窗口。
- 回退 v0.8.0/v0.7.1 前须用 SQLx migration undo 撤销 0005 及其 ledger 记录；只换旧二进制会被 exact migration-state 检查拒绝。
- 删除条数有界不等于耗时严格 O(batch)：索引访问、payload 页回收以及实际引用更新数 R 仍计入成本。
- TTL 按行 expires_at 生效，配置修改不追溯；无 ingest/AI 运行时不清理，过期原文无法 replay，SQLite 文件不自动缩小。
- 此次保持现有配置字段与取值可解析；不实现限流、内层 retry、关闭 dedup 或文件后端。
- 原生 PG 测试依赖与证据位于 `/home/deve/.cache/rss-ai-news-maintenance/`，不属于部署安装；临时服务已停止（pg_ctl status 返回 3）；测试数据与日志保留。

最终 SQL 使用单列 expires_at 排序，避免 PG 对同期限记录额外排序；调整后 SQLite 专项、原生 PG probe 与 workspace Clippy 均重跑通过。

最终日志 SHA-256：
- `affected-tests.log`：`ce3f419f79a21454712e194c5190da43774bb4f7a1e02eb24a889dfa6fdb725d`。
- `runtime-final.log`：`14bb6fefeaae9d47f26b9b14e7668e1fcefaea53fb7f8504a05fe53c432cfc29`。
- `storage-final.log`：`fc1304c49d774ac22d7719fd1d57029027b46d8565f1ec760e4909f823fc41ee`。
- `pg-probe.log`：`44d69bb0ddca1655334c58816a5ebd0f35e3c29ad3e441feaf1c7220845e1bea`。
- `clippy-final.log`：`156d638b63b9826da7120ea92f3ef1708072e6cf3d79550b969380620337ac3c`。

## v0.8.1 发布前补充（2026-09-11）

独立安全审查复现 PG 外键引用行锁可阻塞 artifact 清理：生产默认没有 statement/lock timeout，
SKIP LOCKED 仅覆盖候选 artifact。已在清理事务内设置 1 秒 lock timeout / 5 秒 statement timeout；
超时回滚后继续主流程。新增 PG 锁引用行、保留引用和恢复回归，plan/10 与 AC-P-07 同步。
首次使用 raw_sql 多语句时触发 async_trait / SQLx Executor lifetime 编译约束，改为两个 query 后编译通过。
原生 PG17 actual product API 实测锁等待 1.005 秒返回 55P03，artifact/reference 保留，解除锁后成功清理；
既有升级、并发、续期、外键及回退四组再次通过，临时实例已停止。独立最终代码复核 PASS / no blocker。

发布证据目录：`/home/deve/.cache/rss-ai-news-v0.8.1-release/`。
依赖扫描采用现有 cargo-deny：默认 unmaintained=deny 因既有 fxhash 停止维护而失败；
以既有警告策略 `-W unmaintained` 重跑通过，chacha20/spin yanked 警告保留。依赖图未更改。
最终 local matrix / exact-SHA CI / GHCR 与 GitHub Release 回执见 [v0.8.1 report](../reports/releases/v0.8.1.md)。

最终 v0.8.1 local matrix：4 lanes / 27 steps 全通过，包含完整 workspace build/test 与 fmt/Clippy。

发布完成：tag `v0.8.1` → `69c1722`，CI `34558585394` 五项通过，镜像 workflow `34559035919` 成功。
GitHub Release、两套 immutable image tags 与 stable aliases 读回通过；镜像具体 digest 和执行范围见 release report。
