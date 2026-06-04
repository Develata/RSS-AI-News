# W10 后续：CI 工具链对齐 + migrate 解耦 env 校验

- 日期：2026-05-04
- 作者 / Agent：Claude Code (claude-opus-4-7)
- 分支：main
- 当前 HEAD：508d526
- 相关 commit：
  - `99befaf` ci: bump toolchain to 1.94, parallelize jobs, modernize action versions
  - `7841a0c` ci: address codex review (99befaf) — bump actions to current major + dtolnay@master + Docker patch pin
  - `508d526` fix(config): migrate must not be gated by AI/RSSHub env credential checks
- 相关 tag / release：N/A（v0.1.0 待打）
- 状态：`validated`

## 工作摘要

W10 首次推 GHA 后，三个真问题暴露：(1) CI 工具链 1.88 与本地 1.94 漂移导致 clippy 在远端炸而本地不炸；(2) GHA 不必要地串行；(3) `migration-smoke` job 退出 78 —— `migrate` 命令被 OpenAI / RSSHub 业务凭证校验错误拦截。三个 commit 依次解决，最后一个（`508d526`）是真正的 config 模块设计 bug 修复，不是 CI workaround。

## 影响范围

- crate / 模块：
  - `crates/config/src/{loader.rs,validate.rs,lib.rs}` — 新增 `run_structural_checks` / `load_skip_env_checks` 入口
  - `crates/cli/src/commands/migrate.rs` — 切换至 skip-env-checks 路径
  - `crates/cli/src/commands/publish.rs` — 单点 clippy `uninlined_format_args` 修复
- 真相源对象：N/A
- 额外影响：
  - CI：`.github/workflows/ci.yml`（toolchain pin / 并行化 / 动作版本 / 新增镜像内 `validate-config` 冒烟）
  - Docker：`docker/Dockerfile` 基础镜像 `rust:1.88-slim-bookworm` → `rust:1.94.0-slim-bookworm`（patch pin）
  - 设计：W3 fail-fast 的"每命令作用域"语义首次以代码形式固化（此前只 `Doctor` 用过该模式）

## 关键变更

### 工具链对齐（`99befaf`）

- Dockerfile 与 CI 同步从 1.88 升至 1.94（与本地 host stable 对齐）。引入单一 env `RUST_TOOLCHAIN` 锁定。
- 漂移诊断：1.94 引入的 `clippy::uninlined_format_args` lint 在 CI 触发但本地不触发，因为本地 host 早已是 1.94。修一处 `crates/cli/src/commands/publish.rs:133`：`format!("{} ... {}", a, b)` → `format!("{a} ... {b}")`，1.94 不再有其他命中点。
- clippy 加 `--locked`，保证与 build/test job 走同一 `Cargo.lock`。

### CI 并行化（`99befaf`）

- 取消 `needs:` 链。`lint` / `cargo test` / `migration-smoke` / `docker-build` 现在 t=0 同时启动；此前 `docker-build` 等 `lint`、`migration-smoke` 等 `test`。
- 每个 job 独立 `Swatinem/rust-cache@v2` shared-key（避免缓存互踩）。
- 同 ref 的 `concurrency.cancel-in-progress: true` —— 重 push 时旧 run 自动取消，释放 runner。
- `docker-build` 加 `load: true`，让后续 `docker run` 看到刚 build 的镜像；同时冒烟 runtime 与 debug 两个 stage 的 `--help`。

### Codex 复审反馈实施（`7841a0c`）

Codex 对 `99befaf` 的 verdict 是 `request_changes`，主要意见：

- `actions/checkout` v4 → v6（v6.0.2 是 2026-05-03 当日最新；99befaf 错落在 v5）
- `docker/setup-buildx-action` v3 → v4（v4.0.0 GA）
- `docker/build-push-action` v6 → v7
- `Swatinem/rust-cache@v2`：保留（确认无 v3 release）
- `dtolnay/rust-toolchain@stable` → `@master`：当配置项显式提供 `toolchain` 输入时，`@master` 是仓库 README 推荐写法；`@stable` 在该使用模式下是错误 revision。
- Dockerfile `rust:1.94-slim-bookworm` → `rust:1.94.0-slim-bookworm`（patch-pin），与 CI 的 `RUST_TOOLCHAIN=1.94.0` 真正对齐；否则下一个 1.94.x patch 发布会让镜像 base 静默漂移。
- 新增镜像内 `validate-config` 冒烟（dimension 4 of review）：在 runtime 镜像里 mount 示例 configs 跑 `validate-config`，断言退出码 78（ConfigError）。一次覆盖镜像 entrypoint + clap + W3 fail-fast + 非 root config 读取，且无需任何外部凭证（失败路径本身就是冒烟目标）。

完整复审报告：`.codex-tmp/review_99befaf_codex.md`（未入库，本地工件）。

### `migrate` 解耦 env 凭证校验（`508d526`）

**问题本质**：`config::load` → `validate::run_general_checks` → `collect_env_checks` 这条路径无条件强制 `OPENAI_API_KEY` / `OPENAI_BASE_URL`（当 `ai.enabled=true`）与 `RSSHUB_BASE_URL`（当任何 source 用 `{RSSHUB}` 占位符）。但 `migrate` 命令只打开 SQLite 跑 schema 迁移，与 OpenAI / RSSHub 零耦合。这是 config 模块的设计缺口，不是 CI 配置问题。

**为什么是真 bug 而不是 CI 不便**：W3 fail-fast 的语义本来就是"按命令作用域"——`Publish` 需 `GITHUB_TOKEN`、`AiRun` 需 OpenAI 凭证、`Doctor` 已经走了 per-command 路径。基础设施 / 诊断类命令（`migrate`）此前缺一个等价的 escape hatch。

**改动**：

- `crates/config/src/validate.rs`：新增 `run_structural_checks(app, categories, env)` —— 只跑 schema_version / category 唯一性 / URL 良构 / app value range；跳过凭证存在性检查。
- `crates/config/src/loader.rs`：`load` 抽取私有 `load_inner(..., enforce_env_checks: bool)`；新增 `load_skip_env_checks` public API；`load` 行为不变。
- `crates/config/src/lib.rs`：re-export 新增的 `load_skip_env_checks`。
- `crates/cli/src/commands/migrate.rs`：`run` 与 `check` 都改用 `load_skip_env_checks`，加注释明确设计意图。

**为什么 `validate_feed_url` 在 structural-only 路径下不会误报 `{RSSHUB}` 占位符**：源代码中该函数在遇到 `{RSSHUB}` + 缺 env 时显式 early return（不报错），把"缺 base url"的报告留给 `collect_env_checks`。所以拆分两条路径是干净的。

## 验证与验收

### 自动化验证（本地宿主，rustc 1.94.0）

- `cargo fmt --all -- --check`：通过
- `cargo clippy --workspace --all-targets --locked -- -D warnings`：通过
- `cargo build --workspace --locked`：通过
- `cargo test --workspace --locked`：**342 passed / 0 failed / 0 ignored**（W10 基线 336 + 本次 6 个新测试）

新测试明细：
- `crates/config/src/validate.rs` mod tests：3 个新 case
  - `structural_checks_ignore_missing_openai_env_when_ai_enabled`
  - `structural_checks_ignore_missing_rsshub_env_when_placeholder_used`
  - `structural_checks_still_fail_on_unsupported_schema`
- `crates/config/src/loader.rs` mod tests：3 个新 case（基于 `Workspace` 临时目录 RAII helper + 空 .env）
  - `load_skip_env_checks_succeeds_without_openai_or_rsshub_env`
  - `load_full_fails_without_openai_when_ai_enabled`
  - `load_skip_env_checks_still_fails_on_bad_schema_version`

### 手工验收 — 本地复现 CI smoke 路径（OPENAI_* / RSSHUB_BASE_URL 全 unset）

- `./target/release/rss-ai-news --config-dir configs migrate run` → 退出 0（前为 78）
- `./target/release/rss-ai-news --config-dir configs migrate check` → 退出 0（前为 78）
- `./target/release/rss-ai-news --config-dir configs validate-config` → 退出 78（保持不变，诊断类命令的 fail-fast 行为完整保留）

### GitHub Actions 实跑（push `508d526` 后）

run id `25290911358`，4 job 全绿，total 3m52s：

| Job | 起 → 止 | 用时 | 结果 |
|---|---|---|---|
| `fmt + clippy`              | 21:11:01 → 21:11:32 | 31s   | success |
| `cargo test`                | 21:11:07 → 21:12:18 | 1m11s | success |
| `docker build smoke`        | 21:11:01 → 21:12:42 | 1m41s | success |
| `migration smoke (sqlite)`  | 21:11:01 → 21:14:48 | 3m47s | success |

并行化兑现：3 个 job 在 t=0 同启，`cargo test` 慢 6s（runner 调度抖动，不是 needs 链）。

### 未执行 / 未覆盖

- 端到端业务管线（`ingest → ai-run → publish`）未在 CI 跑（需真实 OpenAI / RSSHub 凭证；不在本次范围）
- `.codex-tmp/review_99befaf_codex.md` 留在本地，不入库（一次性复审工件）

## 结果

- 三个 commit 已 push 到 `origin/main`，GHA 实跑通过
- W10 handoff §"给下一位 Agent" 第 1 条（首次 push + GHA 真跑）满足，可打 `v0.1.0` tag（待用户授权）
- 设计副产品：`load_skip_env_checks` / `run_structural_checks` 是 W3 fail-fast 模式的形式化补全，未来若再有"基础设施类"或"纯诊断类"命令可直接复用

## 风险与后续事项

| 风险 / 后续 | 状态 |
|---|---|
| `v0.1.0` tag 与 GitHub Release 未打 | 待用户决定时点 |
| 端到端管线在 CI 仍未冒烟（需 secrets）| 设计如此，由用户后续决定是否接 GitHub Actions secrets |
| 蓝图 `docs/plan/full-rust-rss-ai-news-blueprint.md` §14 与任务表 W10 节工具链版本若有遗漏文字（W10 handoff 修订追加已澄清"蓝图未写 MSRV 数字"），仍需用户决定是否把 1.94 这个新基线写入设计文档 | 暂搁置 |
| `tests/.gitkeep` 孤儿状态在 W10 修订追加里删除 | 已闭合 |

## 给下一位 Agent 的备注

- 入口文件：
  - `crates/config/src/validate.rs` — `run_structural_checks` 与 `run_general_checks` 的对称对照点
  - `crates/config/src/loader.rs` — `load` / `load_skip_env_checks` / `load_inner` 三段式
  - `crates/cli/src/commands/migrate.rs` — 唯一调用 `load_skip_env_checks` 的地方，附设计意图注释
  - `.github/workflows/ci.yml` — 4 job 并行布局 + 镜像内 `validate-config` 冒烟
  - `docker/Dockerfile` — `rust:1.94.0-slim-bookworm` patch pin
- 历史背景：
  - W10 首次 push 暴露的 3 个问题在 `99befaf` / `7841a0c` / `508d526` 三个 commit 中分层修复；commit message 含完整 root cause 分析
  - 用户既定的多方复审计划（claude + deepseek + codex 逐层 W0-W9 找茬）尚未启动，是后续工作的下一阶段入口
- 继续推进可优先做：
  1. 启动 W0-W9 找茬（用户 driver；本 agent 准备每层提示词，DeepSeek V4 Pro 实施，本 agent 复审 deepseek 结果，用户裁决后实施）
  2. `v0.1.0` tag + Release notes（需用户授权）
  3. 蓝图 / 任务表内的 toolchain 文字若有过时点同步至 1.94
