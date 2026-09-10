# Resource Boundary Follow-up

- 日期：2026-09-10
- 作者：Codex
- 分支：main
- 基准 HEAD：`4c99e5b`
- 相关 commit：pending
- tag / release：N/A
- 状态：`validated`（本地工作区）

## 工作摘要

按用户对 main 的复审意见处理局部资源与诊断边界，保留现有窄依赖、SQLite/PG 双实现、批次 claim、租约和恢复机制。
无依赖、schema、迁移或 Docker 变更。

## 核实与修复

| 反馈 | 核实结果与最终行为 |
|---|---|
| doctor 可写 pool | 成立。改用跨方言 `build_read_only`；shallow/deep 对缺失 SQLite 文件报存储错误且不创建文件，实际连接拒绝写入 |
| provider / event 大诊断 | 成立。AI/GitHub 解码、脱敏后的 provider 正文最多 8 KiB；AI `display_user()` 与最终 event message 最多 16 KiB，包含 UTF-8 安全截断与原字节数标记 |
| Extract/AI 整批 spawn | 成立。滚动 JoinSet 将存活任务数限制为 `http.concurrent_fetches.max(1)`，移除 task 内 semaphore；claim 集合仍为 O(batch size) |
| ingest summary 全量结果 | 成立。所有结果增量累计计数，仅保留最先收集的 32 条业务失败样例；panic/cancel 仍单独计数 |
| 同步渲染超 deadline | 部分成立。Tokio 不能抢占同步工作，但基准代码在单条和批量 target 调用前已经检查 deadline。本次新增 preparation 返回前的检查，让超时明确返回 Timeout |
| doctor secret 明文复制 | 移除提前解包；health 构造器保留 SecretString，只在空值判定与 bearer_auth 时借用明文 |
| PG disk 路径 | disk 明确定义为 SQLite DB 文件系统剩余空间；PG 返回 Info/skip，不拿本地 sqlite_path 推断远端 DB 磁盘 |

诊断截断不会改变 AI quota/model/HTTP 分类：先依据完整消息分类，再限制存储文本。
GitHub 422 的 fast-forward 重试信号若位于截断范围外，会在短诊断前缀中保留；回归覆盖正负分类。
`original_bytes` 表示截断前诊断的字节数（已脱敏，可能含分类前缀），不是 HTTP wire body 长度。
不改成功 response/artifact 留档机制，也不新增非 2xx 原文 artifact 或清理既存大消息。

## 兼容与真相源

- Rust API：`IngestSummary.per_source` 替换为 `failure_samples`；仓库内调用方已同步。外部源码消费者需适配，不能继续依赖完整成功结果集合。
- Rust API：OpenAI/GitHub health 构造器参数由 `Option<String>` 改为 `Option<SecretString>`。
- CLI：`ingest/ai-run --batch-size` 与 `run --ingest-batch-size/--ai-batch-size` 接受 `1..=10000`；越界在参数解析时拒绝。该限制只针对处理批次，不改变 backfill/reindex 批参数。
- 直接调用 flow 的程序仍自行选择批大小；AI 的 lease budget 校验、批内 retryable 后跨 run 重试逻辑不变。
- 已同步 plan/01、02、03、04、07、09、ADR 0009、doctor acceptance case 和 architecture-code 地图；没有新增架构层或依赖方向。

## 验证与验收

- `cargo test --workspace --all-features --no-fail-fast --jobs 3`：通过，836 个独立测试 + 1 次子进程测试，共 837 passed / 0 failed / 62 ignored。
- `cargo build --workspace --all-features --locked`：通过。
- `cargo clippy --workspace --all-targets --all-features --jobs 1 -- -D warnings`：通过。
- `cargo fmt --all -- --check`、`git diff --check`：通过。
- `.ci/check_swallowed_errors.sh`：通过，原有 11 条 allowlist，仅同步 GitHub 原条目的源码行号；未扩大豁免范围。
- `.ci/check_dependency_security_policy.sh`：通过，RSA 无 active dependency path。
- 实际 `target/debug/rss-ai-news` CLI：缺失 SQLite 的 shallow/deep doctor 均 exit 1、不创建文件；显式 migrate 后两者均 exit 0、主数据库 SHA-256 不变；`ingest --batch-size 1000000` 在解析阶段 exit 2。
- 新增/修改 Markdown 链接与 architecture-code Lisp 括号/字符串结构检查：通过。
- 未运行本轮 PostgreSQL/Docker 集成：本机无 PostgreSQL 服务/psql，Docker 仅发现 Windows 路径，未使用；62 个 ignored 中包括环境相关与既有手动/性能项。上一版远端通过不能替代本轮 CI。
- 本轮未重测 benchmark，不给出吞吐/RSS 提升数字。

复现均先看到失败，再核验修复：

| 回归 | 修复前观察 |
|---|---|
| `doctor_missing_database_never_creates_file` | absent.sqlite 被创建 |
| `provider_diagnostics_are_bounded_after_decoding_and_redaction` | 单条诊断 2,000,021 bytes |
| `event_message_is_bounded_after_redaction` | 单条事件消息 2,000,026 bytes |
| `processing_batch_sizes_have_explicit_bounds` | ingest 接受 batch-size=0 |
| Extract/AI `live_tasks_are_bounded_by_concurrency` | 批量 20 / 并发 2 时，持有 context 的 owner 数达 21；修复后最多 flow + 2 tasks |
| `ingest_summary_retains_only_bounded_failure_samples` | 保存 80 个失败结果，而非 32 个样例 |
| `preparation_ready_after_deadline_is_rejected` | 单次 poll 跨过 deadline 后仍返回成功 |
| `diagnostic_cap_preserves_branch_conflict_classification` | 初版截断丢失消息末尾 fast-forward 信号；已保留分类 |

额外验证覆盖实际 doctor pool 的写入拒绝、AI retryable/permanent 大错误写入 SQLite 的 `last_error`/event 字节上限、JSON/plaintext provider 回包、Unicode 边界、短消息不变、汇总全部计数。

## 上一版远端证据

已通过 `gh run view` 核实 [CI run 34492172689](https://github.com/Develata/RSS-AI-News/actions/runs/34492172689)：
HEAD 为 `4c99e5bc13122b72e1ecf711e2ebc58a190b62bd`，`docker build smoke`、`migration smoke (sqlite)`、
`cargo test`、`fmt + clippy`、`test (postgres)` 全部 success。此证据补齐上一轮本地缺口，仅适用于该 HEAD。

## 风险与后续事项

同步渲染仍不可被抢占；post-check 约束后续行为，不保证函数总耗时必在 lease 内。
claim/cleanup 写入继续使用原有数据库失败和租约回收语义。当前 claim 数据、body 与配置仍消耗内存；不宣称整体常量内存或普遍提速。
本轮未重跑性能基线，未提交、push 或部署。后续若提交，按诊断、任务/汇总资源边界等可独立回退的主题组织，不重写既有大提交。
