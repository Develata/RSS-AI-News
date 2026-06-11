# 15 — 重试预算耗尽转终态 + lease 回收接线

本章是 [./08-state-machines.md](./08-state-machines.md) §2.4（"超限后 retryable 失败也会转为
终态"）与 [./11-error-and-recovery.md](./11-error-and-recovery.md) §7.1（"lease 过期后自动回退"）
两条**既有契约**的实现级闭环设计。不引入新状态、新表、新配置——只补齐已承诺但缺位的 transition。

> 渊源：W14-A fallback 收尾时确认 `article_ai_results` 的 retryable 路径在 attempt 耗尽后
> 永久卡 `pending`（lease-abort 分支因此整支撤回，commit 29dcf65）。当时结论：必须作为
> **整条 retryable 路径的统一设计**落地，不塞进单一分支。探查进一步确认 feed_entries /
> publish_records 同构卡死，且 `reclaim_expired_leases` 四个 repo 全部实现但生产路径零调用。

## 1. 现状缺口

### 1.1 缺口一：耗尽卡死（三个 lease 状态机同构）

claim SQL 统一过滤 `attempt_count < max_attempts`；retryable 失败的 release 只把行放回
可领取态、不检查预算。两者夹出一个无人认领的死区：

| 状态机 | 卡死状态 | claim 过滤 | retryable release 行为 |
|---|---|---|---|
| FeedEntry | `pending_fetch`（attempt 已满） | `attempt_count < $max` | 回 `pending_fetch`，不检查 |
| AiResult | `pending`（attempt 已满） | 同上 | 回 `pending`，不检查 |
| Publish | 各阶段态（attempt 已满） | 同上 | 仅清 lease，state 不动，不检查 |

后果：行永久滞留非终态——不再被处理、不进失败统计、运维侧 `pending` 计数说谎、
对应文章从报告中静默消失。

### 1.2 缺口二：reclaim 未接线

`reclaim_expired_leases`（过期 `running` 类 → `pending` 类，清 lease，保 attempt_count）
在 feed_entry / article_ai_result / publish_record / reindex_job 四个 repo 均有实现与测试，
但 **runtime 与 CLI 无任何调用点**。worker 崩溃留下的过期 lease 行永久卡 `running` 类状态；
doctor（`health.rs` Warn + `deep_scan` I8）只检不修。

### 1.3 相关文档漂移（本期一并修正，见 §10）

`RuntimeError::RetryBudgetExhausted` 仅存在于 11 §1 文档、代码中无；配置名
`feed_max_attempts`/`reindex_max_attempts` 与实际 `RetryConfig`
（`feed_entry_max_attempts`/`ai_max_attempts`/`publish_max_attempts`，**无 reindex 项**）不符；
publish 默认值、store_local 重试语义等。

## 2. 设计总览：三件套

```text
flow 启动（首次 claim 前，每 CLI run 一次）
  ├── ① reclaim_expired_leases     过期 running 类 → pending 类（既有方法，纯接线）
  ├── ② terminalize_exhausted      可领取态 + attempt≥max + lease 空/过期 → 终态（新增 sweep）
  └── claim 循环（不变）
        └── 失败 release
              ├── permanent → 终态（不变）
              └── retryable → ③ SQL 内折叠：attempt≥max ? 终态 : 回队（改造）
```

- **③ release 折叠**是主路径：耗尽即时转终态，规则留在 repo 层单一真相源，任何 flow
  都不可能再造出卡死行。
- **② sweep** 兜底两类 ③ 摸不到的行：(a) 本设计落地前已积压的遗留卡死行；
  (b) 崩溃发生在最后一次尝试——reclaim 把行送回 pending 类后预算已满，无 release 可折叠。
- **① reclaim** 是 ② 对崩溃路径生效的前提（先回 pending 类，sweep 才能按谓词收走）。
  顺序固定：① → ②。

## 3. release 折叠（③）

### 3.1 SQL 形态

`release_retryable_failure` 的 state 赋值改为 CASE，并 `RETURNING state` 判定走向：

| 状态机 | 回队态 | 终态 | 预算来源 |
|---|---|---|---|
| FeedEntry | `pending_fetch` | `failed` | `retry.feed_entry_max_attempts` |
| AiResult | `pending` | `permanent_failed` | `retry.ai_max_attempts` |
| Publish | 维持原 state | `failed` | `retry.publish_max_attempts` |

```sql
-- 形态示意（AiResult；feed/publish 同构）
SET state = CASE WHEN attempt_count >= $max THEN 'permanent_failed' ELSE 'pending' END, ...
WHERE id = $id AND lease_owner = $owner
RETURNING state
```

lease guard（`AND lease_owner = $owner`）不变；`last_error` / `last_error_kind` 写**真实底层
错误**（不伪造 "exhausted" kind——耗尽事实由终态 + attempt_count 表达，错误列保留诊断价值）。

### 3.2 签名

```rust
// 返回值从 bool 升级；三个 repo trait 同构
async fn release_retryable_failure(
    &self, id: i64, owner: &str, error: &str, kind: &str,
    max_attempts: u32,            // 新增
    now: OffsetDateTime,
) -> Result<ReleaseFailureOutcome, StorageError>;

pub struct ReleaseFailureOutcome {
    pub released: bool,    // false = lease guard 冲突（行为同现 bool）
    pub exhausted: bool,   // true = 本次折叠进了终态
}
```

flow 据 `exhausted` 发准确事件（见 §7）。调用点共 5 处：`extract.rs`、`ai_run.rs`
（`release_retryable_ai_failure` helper）、`publish/remote.rs` ×2、`publish/store_local.rs`。

### 3.3 一致性论证

claim 过滤 `attempt_count < max` 发生在自增**前** ⇒ claim 后 `attempt_count ≤ max` ⇒
release 时 `attempt_count == max` 当且仅当本次是预算内最后一次尝试。折叠条件
`attempt_count >= max` 与 claim 过滤严格互补，无既不回队也不终态的缝隙。

## 4. terminalize_exhausted sweep（②）

每状态机一个新 repo 方法（reindex 除外，见 §5），谓词：

```sql
UPDATE <table>
SET state = '<终态>',
    last_error = COALESCE(last_error, 'retry budget exhausted'),
    last_error_kind = COALESCE(last_error_kind, 'retry_budget_exhausted'),
    lease_owner = NULL, lease_expires_at = NULL, updated_at = $now
WHERE state IN (<可领取态>)
  AND attempt_count >= $max
  AND (lease_expires_at IS NULL OR lease_expires_at < $now)
```

- 可领取态：feed `('pending_fetch')`；ai `('pending')`；publish
  `('pending','snapshot_frozen','rendered','stored_local')`。
- `COALESCE` 保留行上既有的真实错误（retryable release 已写过）；仅对从未留过错误的行
  （如首次尝试即崩溃）落兜底文案。
- 返回影响行数；>0 时 emit 事件（§7）。
- **配置依赖性（文档化的已知性质）**：sweep 按**当前** max 判断。调低 max_attempts 会让
  存量行按新预算终态化；调高 max 只对尚未终态化的行生效——终态行不自动复活（恢复走 §6）。

## 5. reclaim 接线（①）与各 flow 调用点

规则：**每个 CLI run 在目标表首次 claim 前执行一次 ① + ②**（runtime flow 入口处，
非 CLI 层——库内自洽）。

| flow 入口 | 表 | ① reclaim | ② sweep |
|---|---|---|---|
| extract（含 fetch；ingest 命令内） | feed_entries | ✓ | ✓ |
| ai_run process | article_ai_results | ✓ | ✓ |
| publish 全阶段：freeze / render / store_local / publish_remote（单条 + batch） | publish_records | ✓ | ✓ |
| reindex | reindex_jobs | ✓ | —（无预算语义） |

reindex 不做 sweep：其 claim 不过滤 attempt_count、失败走 `mark_failed` 直转终态，
不存在耗尽卡死；只缺崩溃回收，纯接线 ①。

publish 必须覆盖**全部**阶段入口而非仅 freeze/remote batch：publish CLI 支持断点续跑
（record 停在 `snapshot_frozen` 时跳过 freeze 直接 render；`rendered` → store_local；
`stored_local` → publish_remote），任一阶段都可能是本次 run 的首次 claim。维护操作幂等、
count=0 静默，同一 run 内多次执行无害（codex W15-P4 复审补全）。

## 6. 行为变化与恢复路径（用户已确认的语义收紧）

| 状态机 | 变化 | 恢复路径 |
|---|---|---|
| AiResult | 卡死 `pending` → `permanent_failed`；article 保持 `AiPending`（08 §5.2 不变） | `backfill --target ai`（新 prompt/model 版本行）。**注意**：此前"调高 `ai_max_attempts` 让卡死行复活"的隐性路径随终态化关闭 |
| FeedEntry | 卡死 `pending_fetch` → `failed` | `backfill --target extract`（reset failed）。**改进**：今天的卡死行对 reset 不可见，转终态后可见可救 |
| Publish | 卡死阶段态 → `failed`；后续同 key publish 报 `PublishConflict`（与既有永久失败语义一致，不是新型死路） | bump `render_version`（新 idempotency_key）或人工 |

不受影响：AI-off 直通选稿（`NOT EXISTS` 任意 AI 行，与行状态无关）；`articles.state`
派生规则；软终态不可回退。

## 7. 可观测性

- **release 折叠出终态**：事件升 permanent 级别（如 `ai_failed` level=error、
  `retryable: false`、新增 `budget_exhausted: true`）；run summary 计入永久失败而非"将重试"。
- **maintenance**：①、② 影响行数 >0 时各 emit 一条 run_event
  （`leases_reclaimed` / `retry_budget_swept`，含 count），=0 时静默。
- **doctor**：deep_scan 新增检查"无预算已耗尽的可领取行"（运行间隙的滞留可见；
  sweep 接线后该检查常态应为绿）。既有 I8（过期 running lease）保留。

## 8. 不变契约

1. claim SQL 一字不动（含 `attempt_count < max` 过滤与 PG `FOR UPDATE SKIP LOCKED`）。
2. `attempt_count` 契约不变（11 §4.1：claim 时自增，成功不清零，失败/reclaim 不改值）。
3. `release_permanent_failure` / `release_success*` 语义与签名不变。
4. reclaim SQL 不变（仍只做 running 类 → pending 类，不动 attempt_count）——耗尽判定
   全部收口在 ②③。
5. 终态行的 `last_error*` 保留真实底层错误；不发明覆盖性的合成错误。
6. `articles` 无 lease / 无 last_error 列的派生定位不变。
7. `model_id` 幂等键不可变（W14-A 契约）。

## 9. 实现阶段

| 阶段 | 内容 | 验收 |
|---|---|---|
| P0 | 本章 + 08/11 漂移修正（§10） | 文档自洽 |
| P1 | storage：③ 折叠（3 机 × 2 方言）+ `ReleaseFailureOutcome` + ② sweep（3 机） + 单测 | storage 测试绿 |
| P2 | runtime：5 个 release 调用点 + 各 flow 入口 ①② 接线 + 事件 + 集成测试（含"最后一次尝试 retryable 失败 → 终态"RED-GREEN） | runtime + cli 测试绿 |
| P3 | doctor deep_scan 新检查 + 05/07/operations 文档同步 | doctor 测试绿 |
| P4 | 全量回归 + codex 审查闭环 | fmt/clippy/test/--locked 全绿 |

## 10. 08/11 漂移修正清单（P0 执行）

- 08 §2.4 / 11 §4 配置表：`feed_max_attempts` → `feed_entry_max_attempts`；删除
  `reindex_max_attempts` 行（配置不存在，reindex 无预算语义，见 §5）；publish 默认值
  3 → 5（对齐 `configs/app.toml.example`）。
- 08 §2.3：reclaim 执行时机明确为"各 flow 启动期"（引用本章 §5），非常驻任务。
- 11 §1：`RuntimeError` 树删除 `RetryBudgetExhausted`（代码中不存在；本设计在 repo 层
  折叠，无需 runtime 错误变体）。
- 11 §3 失败路径表：本地落盘行改为"retryable 回原态重试 / 耗尽或永久 → Failed"
  （`store_local.rs` 实有 `is_retryable` 分支）。
- 11 §7.1：补"接线于各 flow 启动期"与本章引用。

代码路径过时时在 [../map/architecture-diff.md](../map/architecture-diff.md) 登记漂移。
