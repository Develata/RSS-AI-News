# 16 — config 版本闭环：active config 跟随真实 sha

> 状态：已确认（2026-06-11，§8 三个决策点全部按推荐采纳：复活迁移 ✓ / 删
> ConfigVersionStore ✓ / needs_sync 刷新版本戳纳入 P2 ✓）。
> 来源：F15-fix6 残留 MEDIUM（bootstrap placeholder 升 active 后真实 config sha 无替换路径），
> [./06-config.md](./06-config.md) §11 已登记为已知缺口。本设计对照代码核实于 2026-06-11，
> 发现缺口比当初记录的更大（见 §1 D2）。

## 1. 问题：config 版本记录是"一次性写入"，不是版本化

`rule_versions` 中 `kind='config'` 行的全部写入口（核实于当前 HEAD）：

| 写入口 | 触发条件 | 写入内容 |
|---|---|---|
| `cli::context_factory::ensure_default_rule_version` | 每次构建 ctx / doctor deps，**仅当无 active config 行** | tag=`cli-default`，payload=真实 `config_sha256`，active |
| `ingest::resolve_source` bootstrap 分支 | 新建 FeedSource 且无 active config 行 | tag=payload=`ingest-bootstrap`，active |
| `reindex::reindex_categories` bootstrap 分支 | reindex 且无 active config 行 | tag=payload=`reindex-categories-bootstrap`，active |
| `RuleVersionRepo::get_or_create_config_version_async`（`ConfigVersionStore` impl） | **生产零调用**（仅测试） | tag=sha 前 12 位，payload=sha |

读路径只有两处：ingest bootstrap 分支与 reindex_categories 给新建/重建的
`feed_sources.config_version` 盖版本戳。

由此确认四个缺陷：

- **D1（原 MEDIUM）placeholder 滞留**：placeholder（`ingest-bootstrap` /
  `reindex-categories-bootstrap`）一旦成为首个 active config 行，
  `ensure_default_rule_version` 永远走"读现有 active"分支提前返回，真实 sha
  **从未被注册**（原记录以为会插成 pending——实际 `get_or_create_config_version_async`
  生产路径根本不被调用）。active config 永远是占位 sha。
- **D2（更广）config 漂移不记录**：用户改 config TOML → 新 sha。seed 只在
  "无 active"时插入，且 tag 固定 `cli-default`；新 sha 永不落库，active 永远是
  **首次启动**的 sha。此后新建的 feed_sources 全部盖旧版本戳，
  "哪个配置生产了这批数据"的审计回答系统性错误。config 版本化目前实质上是
  write-once。
- **D3 死代码 + 文档漂移**：`ConfigVersionStore` trait 及其 impl 生产零调用；
  [./06-config.md](./06-config.md) §11 声称它"向 reindex 流程关联配置快照"、
  `reindex.rs` 注释声称"config loader 启动时已 get_or_create_config_version_async"
  ——均与实现不符。另：`context_factory.rs` / 部分注释引用的
  `docs/design/storage-multi-dialect.md` 文件不存在（登记到 architecture-diff）。
- **D4 不可观测**：doctor 无任何 config 一致性检查（但见 §5——选定方案下该检查
  大部分是死码，本设计**不**新增）。

## 2. 设计原则

- **单一真相源**（宪法 §3.4）：`kind='config'` 的 active 行 == 本次 run 实际
  生效的 config 快照（`LoadedConfig.config_sha256`）。
- **生命周期完整**：config 行需要 supersede 出口；config 回滚（A→B→A）需要
  复活路径。
- 与 reindex 两阶段激活（[./05-storage.md](./05-storage.md) §8、
  [../adr/0004-active-rule-resolver-partial-unique.md](../adr/0004-active-rule-resolver-partial-unique.md)）
  共用同一套 demote/promote SQL 形态，不引入第二种版本切换机制。

## 3. 方案选择

| 方案 | 治 D1 | 治 D2 | 评价 |
|---|:---:|:---:|---|
| (a) admin 手动 `promote-config` 命令 | 手动 | ✗ | 漂移是静默发生的，靠人记得跑命令不治本；不选 |
| **(b) 启动 seed 升级为 sha-keyed rotate** | ✓（自愈存量） | ✓ | **选定**：在唯一汇合点（ctx 构建期 seed）做检测+轮换 |
| (c) doctor 检测 placeholder/mismatch 报 warn | 仅观测 | 仅观测 | 在 (b) 下成为死码（doctor 自身 seed 先行轮换，deep_scan 永远看不到 mismatch），**不做**，理由见 §5 |

## 4. 核心：`rotate_active_config(sha, now)`

storage 层 `RuleVersionRepo` 新增固有方法（替代 `get_or_create_config_version_async`）：

```text
1. SELECT active WHERE kind='config'
   payload_sha256 == sha → NoChange { id }          ← 热路径，单 SELECT
2. 否则单事务内：
   a. demote：UPDATE rule_versions
        SET status='superseded', retired_at=$now
        WHERE kind='config' AND status='active'      → RETURNING id（0 或 1 行）
   b. 按 (kind='config', payload_sha256=$sha) 查既有行：
      - 无 → INSERT (kind, tag=sha[..12], desc, sha, 'active')
      - 有（pending / superseded）→ UPDATE SET status='active', retired_at=NULL
   → Rotated { new_id, demoted_id: Option<i64> }
```

关键决策与不变量：

- **行身份 = payload_sha256**。查找/复用按 payload 而非 version_tag（tag 仅为
  人类可读别名）。tag=sha 前 12 位，`(kind, version_tag)` 唯一冲突（48-bit 碰撞）
  概率可忽略；若真冲突 INSERT 报 Conflict 向上抛、显式 fail，不静默。
- **状态机扩展：仅 `kind='config'` 允许 `superseded → active`（复活）**。
  原因：config 回滚 A→B→A 时 sha 回到 A；`(kind, version_tag)` 唯一 + tag 由
  sha 派生 ⇒ 不能插同 sha 新行（且重复 sha 行会让 BY_SHA 查询不确定），只能
  复用原行并清 `retired_at`。reindex 管理的 kind 不开放此迁移，仍是
  pending → active → superseded 单向（[./08-state-machines.md](./08-state-machines.md) 同步）。
- **demote 永远先行且同事务**：partial unique `uq_rule_versions_kind_active`
  保证至多一行 active；事务保证不出现"0 行 active"被并发读观察到的窗口
  （SQLite 写串行化；PG 下读到旧 active 也只是审计戳落后一拍，无正确性问题）。
- **并发**：双进程同时启动且 sha 不同（窗口极小）→ last-writer-wins，语义正确
  （最近启动的 config 生效）；PG 23505 沿用 `pg_get_or_create` 的单次 retry 模式。

## 5. 接线与生命周期

- `context_factory::ensure_default_rule_version` → 改名
  `ensure_active_config_version`，内部调 `rotate_active_config`。两个既有调用点
  （`build_run_context` / `build_doctor_deps`）覆盖全部建 ctx 的命令；
  `migrate` / `validate-config` 不建 ctx、不触 config 行，维持现状。
- 轮换发生时 `tracing::info!`（demoted_id → new_id, sha 前缀）。不写 run_events：
  seed 在 RunContext 之前执行，无 run_id；rule_versions 行自身即审计记录。
- **ingest / reindex 两个 bootstrap 分支保持不变**（库内嵌/测试场景的兜底）。
  其产生的 placeholder 行自下一次 CLI 启动起被 rotate 收编为 superseded——
  **D1 对存量 DB 自愈，无需迁移脚本**。
- **doctor 不加新 invariant**：doctor 路径 `build_doctor_deps` 自己先 seed
  （即先 rotate），deep_scan 跑到时 mismatch/placeholder 已被修复，检查永远
  空转。"active sha == loaded sha"这个不变量由 seed 本身**构造性保证**，
  比事后检测更强。
- `ConfigVersionStore` trait（config crate）+ `get_or_create_config_version_async`
  + impl：生产死代码，**删除**，消除 config 行的第二写入口（单一真相源）。
  工作区内部 API，无外部消费者。

## 6. 行为变化与边界

| 项 | 变化 |
|---|---|
| `feed_sources.config_version` 语义 | 不变："创建/重建该行时生效的 config 版本" |
| ingest `needs_sync` 更新分支 | 【可选，附带缺口】行被同步更新（display_name/feed_url 等变化）时 `config_version` 不刷新，盖的仍是旧版本戳。建议本期顺带改为 stamp 当前 active（每 run 至多一次 `active_rule` 查询）；不改则登记为已知性质 |
| 历史 feed_sources 行 | 不回填——旧行的版本戳记录的是当时事实，回填反而毁审计 |
| 调回旧 config（A→B→A） | 原 A 行复活（superseded→active，`retired_at` 清空）。复活会丢失上一次 retired_at 的留痕；生效区间的完整历史可由 tracing 日志重建，认为可接受 |
| `06-config.md` §11 | 重写：指向本设计；删除 `ConfigVersionStore` 描述 |

## 7. 分阶段实施

| 阶段 | 内容 | 验收 |
|---|---|---|
| P0 | 本设计文档 + 06-config §11 修订 + architecture-diff 登记两处注释漂移 | 评审通过 |
| P1 | storage：`rotate_active_config` + 双方言 SQL + 测试（首次 seed / 同 sha no-op / 漂移轮换 / 回滚复活 / placeholder 收编 / pending 行 promote / demote+promote 同事务） | `cargo test -p rss-ai-news-storage` 绿 |
| P2 | cli：`ensure_active_config_version` 接线 + tracing + 删 `ConfigVersionStore` 死路径（+ 可选 needs_sync 戳新版本）+ 集成测试（改 sha 重启 → active 跟随；placeholder 库启动 → 自愈） | workspace 回归绿 |
| P3 | 文档同步：05-storage、08-state-machines（superseded→active 仅 config）、06-config、map/architecture-diff 清账 | doc 与实现一致 |

## 8. 决策点（已确认，2026-06-11）

1. **superseded → active 复活**（仅 `kind='config'`）是否接受？替代方案
   （同 sha 插新行）被 `(kind, version_tag)` 唯一约束 + BY_SHA 查询确定性否决。
2. **删除 `ConfigVersionStore` trait** 及 `get_or_create_config_version_async`
   （生产死代码、第二写入口）？
3. **ingest `needs_sync` 分支刷新 `config_version`** 是否纳入本期（P2 可选项）？
