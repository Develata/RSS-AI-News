# 新机器（macOS）接手：audit 红灯、macOS 测试可移植性、D-009

- 日期：2026-11-15
- 作者 / Agent：Claude Code（Opus）
- 分支：main（未推送）
- 当前 HEAD：见 `git log`；基于 09f8fcf
- 相关 commit：730377a（锁文件）、27d88b5（macOS 测试 / 验收 runner）、18d5aa3（D-009 文档）
- 相关 tag / release：N/A（v0.10.0 之后）
- 状态：`validated`（仅本地；未推送，CI 未在这些提交上跑过）

## 工作摘要

2026-10-31 交接后 main 从 5cf1511 走到 09f8fcf，净差异只有 `Cargo.lock`（Dependabot PR #5：tokio 1.54.1、
octocrab 0.54.7；d3a0f6d 与其 revert 1bea8aa 互相抵消）。2026-11-09 的定时 maintenance 因 `cargo audit` 失败。
本轮清掉其中可以只改锁文件解决的两条，在新 macOS 机器上让 `static` + `workspace` 验收转绿，并收敛 D-009。

## 影响范围

- crate / 模块：`tools/acceptance`（runner/process.rs）、`crates/ai` 测试、`crates/decision` 测试；产品代码未改
- 真相源对象：N/A
- 额外影响：`Cargo.lock`；docs（plan/05、plan/09、map/architecture-diff、operations/acceptance-matrix）

## 关键变更

### cargo audit（main 上仍为红）

- 已修（只改锁文件）：quinn-udp 0.5.14→0.5.16（RUSTSEC-2026-0538），在 workspace 内连 dev 依赖路径也没有；
  rustix 1.1.4→1.1.6（RUSTSEC-2026-0486，informational/unsound），没有生产（normal）路径，但经 dev 依赖
  testcontainers → astral-tokio-tar / xattr 可达，xattr 会调用受影响的 `getxattr` / `listxattr`。
  **更正：** 730377a 的提交说明写"两者都只在锁文件中、没有活跃路径"，对 rustix 不准确。当时的 `cargo tree` 只查了根包，
  没有加 `--workspace`。提交未改写，在此更正。
- **未修：libsqlite3-sys 0.30.1，RUSTSEC-2026-0459（CVE-2026-119375，6.8 medium）**。经 sqlx-sqlite 的 `bundled`
  feature 进入活跃依赖（bundled SQLite < 3.54）。修复版本是 libsqlite3-sys ≥ 0.39，但 sqlx 0.8.6 与最新的 sqlx 0.9.0 都要求
  `libsqlite3-sys >=0.30.1, <0.38.0`，**上游尚无可升级路径**。漏洞条件：进程打开一对由不可信方写入的 db 与 `-wal` 文件，
  且二者页大小不同；后果是返回未初始化的内存（信息泄露，本地攻击向量）。"本项目只打开自己 `data/` 下的 DB"属于推断：
  生产配置未读过，也不应读。
- 处理方向需 Develata 决定（见"风险与后续事项"）；`maintenance.yml` 未改动。

### macOS 测试可移植性（此前只在 Linux / WSL 上跑过）

- `tools/acceptance/src/runner/process.rs`：后代进程发现改为一次 POSIX `ps -A -o pid= -o ppid=` 快照后在内存中建树，
  替换只支持 procps 的 `ps --ppid`（macOS 上该命令无输出，只杀直接子进程，`timeout_kills_descendants` 必现失败）。
- `crates/ai/tests/client_tests.rs`：fixture 在 accept 之后显式 `set_nonblocking(false)`。BSD 上 accept 出来的 socket
  会继承监听 socket 的 O_NONBLOCK，导致 `WouldBlock`。
- `crates/decision/tests/contract_tests.rs`：`unreachable_endpoint_is_a_connection_error`。macOS 不回应发往"已绑定但未
  listen"端口的 SYN，表现为 5 秒超时而不是连接被拒；因此非 Linux 平台在连接前先释放该端口，Linux 行为不变。

### D-009（只改文档）

- plan/05：§1 表数改为 12 张 + `_sqlx_migrations`；§2 补 decision 表；§5 claim 注释、§6 方言表（`INTEGER PRIMARY KEY`、
  单语句 claim + 单写者 + busy_timeout）；§7 换成 `migrate.rs` 中五个真实签名；§7.1 列出 0001–0006（0001 实为 9 张表）；
  §7.2 写 run/check 的真实语义；§10 `run_events` 换成 0001 的真实 DDL。plan/09 命令表同步。

## 验证与验收

### 自动化验证（本机 macOS 27.2 arm64，Rust 1.97.0）

- `cargo acceptance run --lane static --lane workspace`：通过（exit 0；fmt / clippy / rustdoc / swallowed-errors /
  dependency-policy / tooling-boundary / acceptance-docs / build / test 全部 Passed）
- 修复前在未改动的 09f8fcf 上：`timeout_kills_descendants` 3/3 失败；`cargo test --workspace --no-fail-fast` 另有
  ai client 2 个、decision 1 个失败。修复后各自重复 3 次均通过
- `cargo audit --ignore RUSTSEC-2023-0071`：仍为 1 个 vulnerability（RUSTSEC-2026-0459），另有 3 条允许的 warning
  （astral-tokio-tar、fxhash unmaintained；chacha20 / spin yanked）
- 未执行：`postgres` / `sqlite` / `release` / `docker` lane（本机 docker 未确认）；Linux 上未在本地复测 process.rs 改动，
  需由 CI（ubuntu）确认

## 结果

- 三个提交可以推送；推送需要 Develata 授权。推送前 maintenance 会一直红，推送后也仍会因 RUSTSEC-2026-0459 保持红灯，
  直到对该漏洞做出决定。

## 风险与后续事项

- codex-reviewer（跨模型）审查结论：无阻断项。已采纳并修正：rustix 依赖路径的表述、`migrate check` 并非无副作用、
  claim SQL 形状的概括、`descendants()` 注释说过头的部分。已接受、未改：非 Linux 平台上 decision 测试在释放端口后、
  连接前存在端口竞争窗口（macOS 临时端口随机分配，碰撞概率极低）；`descendants()` 复杂度为 O(N·D)，进程数为百级，可接受。

- RUSTSEC-2026-0459 的三个选项：(a) 带理由与复查触发条件 `--ignore`，复查触发条件为 sqlx 放开 `<0.38` 上限；
  (b) 改用 `sqlite-unbundled`，链接系统 SQLite，但镜像里的系统 SQLite 也必须 ≥ 3.54；(c) 保持红灯，等 sqlx 发版。
- PR 上跑 cargo audit（原待办 2）：要排在上面的决定之后，否则所有 PR 都会被这条漏洞卡住。
- Phase C、R3 遗留项：未开始，需 Develata 先确认方向。

## 给下一位 Agent 的备注

- 新机器首次运行验收矩阵时，先跑 `cargo acceptance run --lane static --lane workspace`；其余 lane 需要 PG 或 docker。
- `.githooks` 在新 clone 中默认未启用（`core.hooksPath` 为空），提交前需手动跑 fmt / clippy，或按 `.githooks/README.md` 启用。

## 2026-11-15 追加：完整 local profile 与事实更正

### 更正（以本节为准）

- `cargo audit` 允许的 warning 是 **4 条**（astral-tokio-tar、fxhash unmaintained；chacha20、spin yanked），上文写成了 3 条。
- sqlx 的上限：sqlx-sqlite 0.8.6 依赖 `libsqlite3-sys = "0.30.1"`（caret，即 < 0.31）；sqlx-sqlite 0.9.0 依赖 `>=0.30.1, <0.38.0`。
  两者都够不到修复版本 ≥ 0.39。sqlx 0.8.6 有 `sqlite-unbundled` feature（已在 registry 的 `Cargo.toml` 中核实），但对应的系统 SQLite
  同样需要 ≥ 3.54。
- "macOS 临时端口随机分配、碰撞概率极低"是推断。补充证据：decision 的完整 `contract_tests` 二进制（32 个测试并行）连续运行 20 次，
  0 失败。
- "其余 lane 需要 PG 或 docker"不对：`local` profile 中的 `sqlite` / `release` lane 都不需要；只有 `postgres` / `docker` lane 需要
  （本机 docker daemon 未运行）。

### 新增文档修正

- plan/05 §5 的 lease 表清单补上 `decision_results`（0006 有 `lease_owner` / `lease_expires_at`）。

### 验证（HEAD = 本节所在提交的父提交 + 本节文档改动）

- `CARGO_PROFILE_RELEASE_STRIP=none cargo acceptance run --profile local --expected-version 0.10.0`：**Passed**
  （static / workspace / sqlite / release 四条 lane）。
- 不带该环境变量时，release 构建失败：dyld 拒绝加载 `target/release/deps/libsqlx_macros-*.dylib`，报 "mis-aligned LINKEDIT
  string pool"；`cargo clean -p sqlx-macros` 后可复现，把 proc-macro 的 opt-level 设为 0 后同样失败，只有设 `strip=none` 才通过。
  结论：本机（macOS 27.2、CLT `ld-27106`、`/Library/Developer/CommandLineTools/usr/bin/strip`）上，release profile 默认的
  `strip = "debuginfo"` 会弄坏 proc-macro dylib。属于机器环境问题，与仓库无关；仓库未改动，Linux / CI / docker 不受影响。
