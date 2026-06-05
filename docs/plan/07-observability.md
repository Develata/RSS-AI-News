# 07 — 可观测性

本章详解可观测性栈：tracing 日志 / metrics / run_events 事件流 / health check / doctor 子命令。

宪法 §3.5 要求"失败路径与可观测性内建于骨架"。本章是该原则的物理实现。

## 1. 边界

本章覆盖：
- `tracing` 订阅者初始化与日志落盘
- `MetricsRecorder` 抽象与 Prometheus 暴露
- `run_events` append-only 事件流（已在 [./05-storage.md](./05-storage.md) §10 介绍 schema，本章讲发射与消费）
- `HealthCheck` trait 与 `doctor` 子命令
- 密钥redaction 在三条出口的统一执行

**不覆盖**：
- 错误类型本身 → [./11-error-and-recovery.md](./11-error-and-recovery.md)
- 重试与失败恢复 → 同上
- 部署侧的 metrics 端口暴露 / 日志卷挂载 → [./12-deployment.md](./12-deployment.md)

## 2. 三条出口

可观测信号有三条彼此正交的出口：

| 出口 | 用途 | 写入位置 | 消费方 |
|---|---|---|---|
| **tracing 日志** | 行内事件 / 调试轨迹 | stderr + 日轮转文件 | 人 / log 收集器 |
| **metrics** | 数值时序 | 进程内 `MetricsRecorder` → 可选 Prometheus 端点 | Prometheus / Grafana |
| **run_events 表** | 业务里程碑（持久化） | `run_events` 表（append-only） | 自家 CLI 查询 / 复盘 |

三条出口共享同一套**密钥redaction 过滤器**（`crates/observability/src/redact.rs`），保证
密钥不会在任一通道泄漏。

## 3. tracing 日志

入口：[`crates/observability/src/tracing_init.rs::init`](../../crates/observability/src/tracing_init.rs)。

### 3.1 行为

- `[observability].log_level` → `EnvFilter`。`RUST_LOG` 环境变量可覆盖
- `[observability].log_format ∈ {"pretty", "json"}` → 切换 fmt layer
- `[observability].log_file`（或 `--log-file`）非空 → 同时写 stderr + 日轮转文件
  - `tracing_appender::rolling::daily`，按 `prefix.YYYY-MM-DD` 命名
  - 路径解析规则（`parse_log_file_path`）：
    - 空串 → 仅 stderr
    - `logs/app.log` → 目录 `logs/`，prefix `app.log`
    - 纯文件名 `app.log` → 当前目录 + prefix `app.log`
    - 无 file_name（`.` / `..` / 根）→ raw 作目录 + fallback prefix `"rss-ai-news"`
- 启动前 `create_dir_all`：父目录不存在/无权限 → 立即降级 stderr 并 `eprintln!` 一行 warning
- 多次 `init` 时只有第一次生效（`try_init`）；后续调用：
  - file appender 模式 → drop guard 让 worker 退出，无资源泄漏
  - 任一失败模式 → `eprintln!`（非 `tracing::warn!`，因 subscriber 状态此刻不可信）

### 3.2 WorkerGuard 生命周期

`init` 在 file 模式下返回 `Option<WorkerGuard>`。**调用方必须持有 guard 到进程结束**：
`tracing-appender` 的 non-blocking writer 通过 channel 向后台 worker 投递日志，
guard 在 `Drop` 时 flush 剩余消息 + 关闭文件。提前 drop 会让进程退出前的末尾日志被截断。

CLI 在 `main` 顶层把 guard 绑定到栈变量，作用域覆盖整个 run，详见
[`crates/cli/src/lib.rs`](../../crates/cli/src/lib.rs)。

### 3.3 命名约定

- span：`stage=ingest`、`stage=extract` 等覆盖五大流水线段
- field：`run_id`、`category=ai`、`source=openai_blog`、`entry_id=...`
- level：`error`（必须人工介入）/ `warn`（异常但有兜底）/ `info`（里程碑）/ `debug`（开发期）/ `trace`（高频）

## 4. metrics

抽象：[`crates/observability/src/metrics.rs::MetricsRecorder`](../../crates/observability/src/metrics.rs)，
三个方法：

```rust
fn counter_inc(&self, name: &'static str, labels: &[(&str, &str)], value: u64);
fn histogram_observe(&self, name: &'static str, labels: &[(&str, &str)], value: f64);
fn gauge_set(&self, name: &'static str, labels: &[(&str, &str)], value: f64);
```

三种实现：

| 实现 | 用途 |
|---|---|
| `NullMetrics` | 默认；零开销 no-op，禁用 metrics 时使用 |
| `InMemoryMetrics` | 测试用；`Mutex<HashMap>` 累计，可断言 `counter_total` / `histogram_samples` / `gauge_value` |
| `PrometheusMetrics` | 生产用；底层 `prometheus` crate，按 `name + sorted(labels)` 聚合 |

启用条件：`[observability].enable_metrics = true` ⇒ 注入 `PrometheusMetrics` 并 spawn
`serve_metrics`（[`crates/observability/src/prometheus.rs`](../../crates/observability/src/prometheus.rs)）
监听 `[observability].metrics_bind`，暴露 `GET /metrics`。

label key 强制 `&'static str`，防止运行时拼接动态 label 名导致 cardinality 爆炸。
value 是 `&str`，允许动态（如 category key）；高 cardinality 字段（entry_id / article_id）
**不应**作 label，应进 `run_events`。

## 5. run_events 事件流

schema 详见 [./05-storage.md](./05-storage.md) §10。本节讲发射与字段语义。

入口：[`crates/runtime/src/events.rs::RunEventEmitter`](../../crates/runtime/src/events.rs)。
所有发射经 `RunEventEmitter::emit`：

```rust
emitter.emit(
    event_kind,        // 如 "feed_source_fetch_succeeded" / "entry_dedup_skipped"
    severity,          // "info" / "warn" / "error"
    target_kind,       // Some("feed_source") / Some("article") / None
    target_id,         // Some(i64) 主键 / None
    message,           // 人类可读
    context,           // Option<serde_json::Value>，自由结构
).await;
```

### 5.1 强制redaction

`context` 写入 `run_events.context_json` 前**必须**经
`rss_ai_news_observability::redact::redact_event_context` 过滤：

- URL userinfo（`https://user:pass@host`）→ `user:***@host`
- `Authorization: Bearer ...` header → `Bearer ***`
- JSON 内键名匹配 `api_key|token|secret|password|access_key`（不区分大小写）→ 值替换为 `***`

redaction 在**截断之前**执行，保证即使内容超长，密钥也已被遮蔽。

### 5.2 截断

`CONTEXT_JSON_MAX_BYTES = 4096`。超长 context 被替换为：

```json
{"truncated": true, "preview": "<前 3500 字节>"}
```

### 5.3 持久化失败的语义

`repo.insert` 失败时**仅** `tracing::error!` 一行，**不**向上抛错。理由：
run_events 是观测旁路，主流程不应因事件写入失败而退出。这是宪法 §"不静默吞错"的
**唯一豁免点**（"日志写入失败"），详见 [./11-error-and-recovery.md](./11-error-and-recovery.md) §豁免清单。

### 5.4 查询入口

`run_events` 没有内置查询子命令，按 SQL 直查即可。typical query：

```sql
SELECT event_kind, severity, message, context_json, created_at
FROM run_events
WHERE run_id = ? AND stage = 'ai_run'
ORDER BY id;
```

## 6. HealthCheck 与 doctor 子命令

抽象：[`crates/observability/src/health.rs::HealthCheck`](../../crates/observability/src/health.rs)：

```rust
#[async_trait]
pub trait HealthCheck: Send + Sync {
    fn name(&self) -> &str;
    async fn run(&self) -> CheckOutcome;  // Ok / Warn / Fail / Info
}
```

`doctor` 子命令（[`crates/cli/src/commands/doctor.rs`](../../crates/cli/src/commands/doctor.rs)）依次执行所有注册的
check，汇总成 `CheckReport`，按 outcome 染色输出。

当前注册的 check（按代码顺序）：

| name | 验证项 |
|---|---|
| `config` | `validate-config` 的全套检查（含 publish + env） |
| `database` | 连接 + 简单 `SELECT 1` |
| `migrations` | 已 apply 与 embedded 一致性 |
| `openai` | `[ai].enabled` 为 true 时探测 `OPENAI_BASE_URL` 可达 |
| `github` | 远端 publish 启用时探测 `https://api.github.com/repos/<owner>/<repo>` |
| `rsshub` | 任一 source 使用占位符时探测 `RSSHUB_BASE_URL` 健康端点 |
| `disk` | `local_output_dir` + `[artifact].file_storage_dir` 可写 |

`doctor` exit code 表：

- 全部 `Ok` 或仅 `Info` → exit 0
- 含 `Warn` 不含 `Fail` → exit 0（仅输出 warning）
- 含 `Fail` → exit 1（`DoctorFailed` → RuntimeError；详见 [./11-error-and-recovery.md](./11-error-and-recovery.md) §5）

### 6.1 显示侧redaction

`CheckReport` 内 message 经过 `redact_authorization_header` + `redact_url_userinfo` 两层过滤后才输出。

## 7. 故障注入与回归

`InMemoryMetrics` + 自定义 `MockHealthCheck` 是测试侧 doctor 行为的标准组合。
metric 与 event 的发射在大部分 flow 测试中作为 assertion 出现：

```rust
let metrics = Arc::new(InMemoryMetrics::default());
// run flow ...
assert_eq!(metrics.counter_total("ai_request_total", &[("category", "ai")]), 3);
```

测试覆盖见各 crate 的 `tests/` 目录与 `crates/runtime/tests/`。

## 8. 当前实现入口

| 内容 | 路径 |
|---|---|
| tracing 初始化 | [`crates/observability/src/tracing_init.rs`](../../crates/observability/src/tracing_init.rs) |
| MetricsRecorder + 三种实现 | [`crates/observability/src/metrics.rs`](../../crates/observability/src/metrics.rs) |
| Prometheus 暴露 | [`crates/observability/src/prometheus.rs`](../../crates/observability/src/prometheus.rs) |
| 密钥redaction 过滤器 | [`crates/observability/src/redact.rs`](../../crates/observability/src/redact.rs) |
| HealthCheck trait + CheckReport | [`crates/observability/src/health.rs`](../../crates/observability/src/health.rs) |
| RunEventEmitter（发射 + redaction + 截断） | [`crates/runtime/src/events.rs`](../../crates/runtime/src/events.rs) |
| run_events repo | [`crates/storage/src/repo/run_event.rs`](../../crates/storage/src/repo/run_event.rs) |
| doctor CLI | [`crates/cli/src/commands/doctor.rs`](../../crates/cli/src/commands/doctor.rs) |
| WorkerGuard 持有点 | [`crates/cli/src/lib.rs`](../../crates/cli/src/lib.rs) |

代码路径过时时在 [../map/architecture-diff.md](../map/architecture-diff.md)登记漂移。
