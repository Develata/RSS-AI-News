# typed-decision Phase A：独立 decision crate（Jev / Laya）

- 日期：2026-10-31
- 作者 / Agent：Claude Code（Opus）
- 分支：`feat/decision-phase-a`（worktree，基于 `0c356ac`）
- 相关 commit：ADR `6cdb84d`、`688147d`、`feb12f6`、`400d089`；实现 `97b571d`；第二轮审查修复 `600eee0`
- 相关 tag / release：N/A
- 状态：`validated`

## 工作摘要

按已接受的 ADR 0010 实现 Phase A：新增能力层 crate `rss-ai-news-decision`，支持 Jev（托管）与 laya-server
（自部署）两个 typed-decision provider。默认关闭，不接入任何 flow、不建表、不加子命令。用户附加要求"整个功能
可干净移除"：全部实现在 `crates/decision`，其它 crate 只有配置透传与 validate-config 两处入口。

## 影响范围

- crate / 模块：新增 `crates/decision`；config（`decision.rs` 透传、`LoadedConfig.decision`）；cli（validate-config
  调用 `validate_sections`）；5 处 `LoadedConfig` 字面量补字段
- 真相源对象：N/A（无表、无迁移）
- 额外影响：config schema 新增可选段 `[decision]` 与 `[[category.decision.questions]]`；`.ci/swallowed-error-allowlist.txt`
  中 `loader.rs` 条目行号 425 → 435

## 关键变更

### decision crate

- `question.rs`：`QuestionSpec` 与共享规范化 `normalized()`；校验、请求编码、`policy_version` 都用规范化结果。
- `policy.rs`：规范化 JSON 的 SHA-256，两组固定向量（`sha256sum` 独立核对）。
- `provider/{jev,laya}.rs`：各自的线格式、限制、精度 ε（Laya 5e-5，Jev 暂定 5e-3）、必填字段与复算规则；
  Laya 禁用 `auto`，拒绝选项塌缩。
- `client.rs`：`SystemOneClient`，`POST {base_url}/v1/systemone`；不跟随重定向；1 MiB 响应上限；错误体读取
  失败时保留 status；`Retry-After` 只认十进制秒；`use_env_proxy` 控制是否遵从环境代理。
- `error.rs`：闭合错误分类，HTTP 408 为 `RequestTimeout`、本地超时为 `Timeout`；错误码白名单，不回显错误体。
- `config.rs`：两类配置段的解析与校验、`build_client`（关闭时不解析凭据）；配置的类型错误不回显输入值。
- Debug 脱敏：`ClientSettings` / `DecisionSettings` 只打印 origin，`DecisionSections` / `CategorySection` 不打印原始 TOML；
  `base_url` 不得含用户信息、query、fragment。

### 审查

- Codex（gpt-6-astra，high）两轮代码审查。第一轮 1 BLOCKER（Debug 泄露 base_url / 原始 TOML）+ 7 SHOULD_FIX +
  1 NICE_TO_HAVE，全部在 `97b571d` 修复；第二轮无 BLOCKER，3 SHOULD_FIX（配置类型错误回显值、`CategorySection`
  Debug、覆盖描述过宽）+ 3 NICE_TO_HAVE，全部在 `600eee0` 修复。每条均先核实（配置回显以 canary 测试复现）。

## 验证与验收

### 自动化验证（`600eee0`）

- `cargo fmt --all --check`：通过
- `cargo clippy --workspace --all-targets -- -D warnings`：通过
- `cargo test --workspace --no-fail-fast`：978 passed / 0 failed / 69 ignored
- `cargo acceptance run --lane static`：通过（fmt、clippy、rustdoc、swallowed-errors、dependency-policy、
  tooling-boundary、acceptance-docs）
- 把 `HTTP(S)_PROXY` / `ALL_PROXY` 设为失效代理、清空 `NO_PROXY` 后 `cargo test -p rss-ai-news-decision`：
  36 单元 + 25 契约全部通过

### 手工验收

- Laya 真机探针（`crates/decision/tests/live_probe.rs`，用户提供的自部署 laya-server，1 次请求，model
  `multilingual`）：通过。`reported_model = laya-rl-agent`、`routing.model = multilingual`、
  `usage.truncated = false`、`state_tokens_dropped = 0`、`input_tokens = 322`；概率最多 4 位小数，与 ε = 5e-5 一致；
  三题答案与样例文章相符（非营销、release、notable）。
- Jev 真机探针：未执行（等待用户提供 key）。

## 结果

- 可合并：默认关闭，未配置 `[decision]` 时生产行为不变；配置段有错也只影响 validate-config。
- 用户影响：可在 app.toml 加 `[decision]`、在板块加问题，validate-config 会校验；Phase A 不会真正调用。

## 风险与后续事项

- Jev 精度 ε 仍为暂定 5e-3，需真机探针实测后收紧（ADR §4、plan/18 §3）。
- Phase B：独立 `decision-run` 命令、表与 lease、执行来源记录，需另写 ADR。
- 响应无 Content-Length 时的流式上限分支没有单独用例（plan/18 §7 已注明）。
- worktree 根目录的 `.env`（gitignored）存放 Laya 探针 key，仅本机使用。

## 给下一位 Agent 的备注

- 入口：`docs/adr/0010-typed-decision-capability.md`、`docs/plan/18-typed-decision.md`、`crates/decision/src/lib.rs`。
- 探针用法见 `crates/decision/tests/live_probe.rs` 顶部注释；key 从进程环境或工作区 `.env` 读取，不打印。
- 回环 mock 测试必须用 `use_env_proxy: false`：reqwest 不匹配 `127.*` 形式的 `NO_PROXY`。
