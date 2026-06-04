# 11 — 错误模型 + 失败路径 + 恢复机制

本章是宪法 §5（失败 → 观测 → 验证内建）的实现级契约。说明：
1. 三层错误枚举
2. 错误元信息接口（retryability / error_kind / 用户面板）
3. 失败路径与重试边界
4. exit code 速查
5. "绝不静默吞错"三层 enforcement

状态机失败分支的详细 transition 表在 [./08-state-machines.md](./08-state-machines.md)。本章
不重复，只讲错误模型与跨能力的恢复机制。

## 1. 三层错误枚举

错误按调用层分层。下层不依赖上层。

```text
能力执行层（各 crate 内部）
├── FeedError        (crates/feed)
├── ExtractorError   (crates/extractor)
├── AiError          (crates/ai)
├── StorageError     (crates/storage)
├── PublishError     (crates/publish)
└── ReportError      (crates/report)

流程协调层（crates/runtime）
└── RuntimeError
    ├── Feed(FeedError)
    ├── Extractor(ExtractorError)
    ├── Ai(AiError)
    ├── Storage(StorageError)
    ├── Publish(PublishError)
    ├── Report(ReportError)
    ├── LeaseConflict { table, id, expected_owner }
    ├── RetryBudgetExhausted { table, id, attempts }
    └── Cancelled

交互层（crates/cli + binary）
└── AppError
    ├── Config(ConfigError)
    ├── Runtime(RuntimeError)
    └── Io(std::io::Error)
```

### 1.1 不允许的跨层包装

- 能力层错误**不**包装上层错误（避免循环依赖）
- 流程层错误**不**直接持有 CLI 错误
- 不允许用 `Box<dyn Error>` 抹平类型（违反"显式错误"原则）

实现策略：每层 crate 用 `thiserror` 派生自己的 enum；上层用 `#[from]` 自动转换。

## 2. 错误元信息接口

每层底层错误必须实现：

```rust
trait ClassifiedError {
    fn is_retryable(&self) -> bool;
    fn error_kind(&self) -> &str;       // 存入 last_error_kind 列
    fn display_user(&self) -> String;   // CLI 用户可读
    fn display_debug(&self) -> String;  // 日志/诊断详情
}
```

字段约定：
- `is_retryable()` — runtime 用它决定状态转移目标（回 pending vs 转 failed）
- `error_kind()` — 短字符串（如 `"http_timeout"` / `"parse_failed"`），写入数据库 `last_error_kind`
  列，便于聚合分析
- `display_user()` — 用户友好的简短消息，不暴露内部路径
- `display_debug()` — 完整 chain，含 source error 与上下文

## 3. 失败路径分类

按对象/状态机分类，详细 transition 见 [./08-state-machines.md](./08-state-machines.md)。

| 失败发生位置 | 持久化字段 | 状态变化 |
|---|---|---|
| feed 抓取 | `feed_entries.last_error*` | `Fetching → PendingFetch`（retry）/ `Failed`（exhausted） |
| 正文提取 | `feed_entries.last_error*` | 同上；fallback 命中时转 `FallbackPersisted` |
| AI 调用 | `article_ai_results.last_error*` | `Running → Pending`（retry）/ `PermanentFailed`（exhausted） |
| 本地落盘 | `publish_records.last_error*` | `Rendered → Failed`（无 retry） |
| GitHub 推送 | `publish_records.last_error*` | 422 → 重试一次后 `Failed`；其它直接 `Failed` |
| Reindex 批次 | `reindex_jobs.last_error*` | `Running → Pending`（retry）/ `Failed`（exhausted） |

`articles` 表**不**承载 `last_error*` 列。错误归属规则见 [./08-state-machines.md §4.2](./08-state-machines.md)。

## 4. retry 预算

每个状态机有独立预算，超限即转终态 `failed`。配置项：

| 状态机 | 配置 | 默认 |
|---|---|---|
| FeedEntry | `retry.feed_max_attempts` | 5 |
| AiResult | `retry.ai_max_attempts` | 3 |
| Publish | `retry.publish_max_attempts` | 3 |
| Reindex | `retry.reindex_max_attempts` | 3 |

详细 schema 见 [./06-config.md](./06-config.md)。

### 4.1 attempt_count 的契约

- claim 时递增：`UPDATE ... SET state='running', attempt_count = attempt_count + 1`
- 成功时**不**清零
- 失败时**不**改值（已在 claim 时递增）
- reclaim（lease 过期）时**不**改值
- 这样保证：worker 崩溃 / lease 过期不会刷预算，预算严格反映尝试次数

## 5. exit code 速查

CLI 退出码遵循 sysexits.h 约定 + 业务扩展：

| Code | 来源 | 含义 |
|---|---|---|
| 0 | 成功 | 全量成功，或含部分非致命跳过 |
| 1 | 业务失败 | 全量失败 / 不可恢复 |
| 64 | EX_USAGE | CLI 参数错（clap 自动） |
| 65 | EX_DATAERR | 数据 / 协议错（如 schema drift） |
| 74 | EX_IOERR | I/O 错（DB / 文件系统） |
| 78 | EX_CONFIG | 配置错（schema / 必填缺失 / 非法值） |

特殊：
- `migrate` / `validate-config` 配置错返 **78**，便于 CI / Docker scheduler 区分"配置问题"vs"业务问题"
- `reindex --dry-run` 即使数据有不一致也返 0（仅打印 plan，不写库）

## 6. "绝不静默吞错" 三层 enforcement

宪法 §5.3 的硬约束。三层防御：

### 6.1 编译期（workspace lint）

`Cargo.toml` 根 `[workspace.lints]` 配置：

```toml
[workspace.lints.rust]
unused_must_use = "deny"

[workspace.lints.clippy]
let_underscore_must_use = "deny"
let_underscore_future = "deny"
ok_expect = "warn"
ignored_unit_patterns = "warn"
```

任何 `let _ = fallible_call()` 或忽略 `Result` 都不编译过。

### 6.2 运行时（runtime swallow test）

每个 Flow 的 fallible 路径必须命中以下之一才允许退出：
- **emit**：tracing::error / tracing::warn
- **propagate**：return Err(...)
- **persist**：写入 `run_events` / `last_error*`

`crates/runtime/src/error.rs` 中的契约测试覆盖每个 Flow 的失败路径。

### 6.3 CI（ripgrep 扫描）

`.ci/check_swallowed_errors.sh` 扫描两种危险模式：

- 模式 A：`if let Ok(...) = ...`（忽略 Err 分支）
- 模式 B：`.ok();\s*$`（丢弃 Err 转 Option 后不用）

任一非空匹配 fail CI。豁免名单在 `.ci/swallowed-error-allowlist.txt`，每条豁免必须附理由。

## 7. 跨能力恢复机制

### 7.1 lease 过期回收

`fetching` / `extracting` / `running` / `running`（reindex）期间崩溃 → lease 过期后自动回退到
前置 `pending` 态，下轮 claim 重领。详见 [./05-storage.md](./05-storage.md) §lease。

### 7.2 事务回滚

跨表写入（如 `articles` INSERT + `feed_entries` UPDATE）在同一事务。任一失败 → 整事务回滚，
状态机保持原状。

### 7.3 publish 的批量回滚

`publish_records` 进入 `Rendered → StoredLocal → PublishedRemote` 是顺序状态。任一步失败：
- `StoredLocal` 失败：record 转 `Failed`；不影响 GitHub
- GitHub 推送 422 lost-update：retry 一次（详见 [../adr/0003-publish-snapshot-immutable.md](../adr/0003-publish-snapshot-immutable.md)（建设中））
- GitHub 推送最终失败：record 转 `Failed`；本地文件保留

### 7.4 backfill 作为人工恢复入口

如果状态机进入 `failed` 终态，但产品上需要重处理：
- `backfill --target extract` 重跑提取（限 `feed_entries.state = failed`）
- `backfill --target ai` 重跑 AI（基于新 prompt_version）

软终态（`dedup_skipped` / `fallback_persisted` / `publish_skipped`）**不**接受 backfill 改写。

## 8. 观测点接入

每个错误必须在以下三处之一留痕：

| 通道 | 何时使用 |
|---|---|
| `tracing::error!` / `warn!` | 一般性可重试错误（不堵塞流） |
| `run_events` 表 | 关键持久事件（source 失败、article 永久失败、publish 失败） |
| `last_error*` 列 | 状态机持久 last 错误 |

详细观测设计见 [./07-observability.md](./07-observability.md)。

## 9. 当前实现入口

| 内容 | 路径 |
|---|---|
| RuntimeError | [`crates/runtime/src/error.rs`](../../crates/runtime/src/error.rs) |
| 各能力 crate 错误 | `crates/{feed,extractor,ai,storage,publish,report}/src/error.rs` 或同级 |
| AppError + exit code | [`crates/cli/src/lib.rs`](../../crates/cli/src/lib.rs) + `crates/cli/src/error.rs` |
| workspace lint | [`Cargo.toml`](../../Cargo.toml) `[workspace.lints]` |
| CI 扫描脚本 | [`.ci/check_swallowed_errors.sh`](../../.ci/check_swallowed_errors.sh) |
| 豁免名单 | [`.ci/swallowed-error-allowlist.txt`](../../.ci/swallowed-error-allowlist.txt) |

代码路径过时时在 [../map/architecture-diff.md](../map/architecture-diff.md)（建设中）登记漂移。
