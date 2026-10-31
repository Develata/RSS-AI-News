# 18 — typed-decision 能力（Phase A）

决策与一手来源见 [../adr/0010-typed-decision-capability.md](../adr/0010-typed-decision-capability.md)（accepted）。
本章是 Phase A 的实现契约。Phase A 不接入任何 flow、不建表、不加子命令；默认关闭时生产行为零变化。

## 1. 边界与可移除性

- 全部实现在 [`crates/decision`](../../crates/decision/)（能力层，只依赖 domain 已有的 `SecretString` /
  `ClassifiedError`，不修改 domain）。
- 其它 crate 只有两处入口：
  - config：`LoadedConfig.decision`（[`crates/config/src/decision.rs`](../../crates/config/src/decision.rs)）原样透传
    `[decision]` 与 `[category.decision]` 为 `toml::Value`；
  - cli：validate-config 调用 `rss_ai_news_decision::validate_sections`。
- 整体移除：
  1. 删 `crates/decision` 与 workspace 条目；
  2. 删上述透传模块、`LoadedConfig.decision` 及 5 处 `LoadedConfig` 字面量中的该字段；
  3. 删 validate-config 中的 `decision_warnings`；
  4. 删本章、ADR 0010 链接与配置文档段。

  没有表、没有迁移。
- 单删一个 provider：删 `crates/decision/src/provider/{jev,laya}.rs` 之一与 `Provider` 的对应变体。

| 模块 | 职责 |
|---|---|
| `question.rs` | `QuestionSpec` / `QuestionKind` / `ChoiceOption`；公共校验 `validate_common` |
| `policy.rs` | `canonical_policy` / `policy_version`（规范化 JSON 的 SHA-256） |
| `provider/mod.rs` | `Provider` 枚举、`check_model`、`validate_questions`、编码 / 解码分发、答案配对 |
| `provider/jev.rs` / `provider/laya.rs` | 各自的线格式、限制、精度 ε、必填字段与复算规则 |
| `response.rs` | 归一化结果类型、共享线格式、拒绝重复键的 `UniqueMap`、取值域 / 分布 / 一致性检查 |
| `error.rs` | 闭合错误分类 `DecisionError`、status 映射、白名单错误码 |
| `client.rs` | `DecisionClient` trait、`SystemOneClient`（`POST {base_url}/v1/systemone`） |
| `config.rs` | 两类配置段的解析与校验、`build_client`、`validate_sections` |

## 2. 请求

- 调用方构造 `DecisionRequest { title, body, questions }`。
- 问题先经 `QuestionSpec::normalized`（字符串 trim，空白的可选释义视为未填）；本地校验、请求编码与
  `policy_version` 都用这一结果，配置解析也产出同一形式。
- body 按 `max_state_chars`（Unicode 字符）截断后，以 `state = {"title", "body"}` 发送。这只是预截断，不保证满足
  provider 的 token 限制。
- 发送前先在本地校验问题，不合规即 `InvalidRequest`，不发请求：
  - 问题列表为空；
  - 违反公共规则或 provider 限制。

| | Jev | Laya |
|---|---|---|
| 请求字段 | `state`、`model`、`questions` | `state`、`questions`、`model`、`max_len`（恰好四个） |
| noul 释义 | `criteria` 只含已填的 `"true"` / `"false"`，都未填则省略（两家文档 / 源码均允许单侧） | 同左 |
| choice 无释义 | `null` | `""`（laya-server 编码为纯 label） |
| score | `criteria = [levels…]` | 同左 |
| `max_len` | 不发 | 总是发送（缺省 2048） |

- 传输：不跟随重定向；`ClientSettings.use_env_proxy` 为 true 时遵从 `HTTP(S)_PROXY` / `NO_PROXY`
  （配置构造的 client 总是 true，只有回环 mock 测试关闭）。
- `base_url` 必须是带 host 的 http(s)，且不含用户信息、query、fragment；`ClientSettings`、`DecisionSettings`
  与 `LoadedConfig.decision` 的 Debug 只打印 origin / 段是否存在，不打印路径与原始 TOML。

## 3. 校验规则

- **公共**：
  - 均在规范化之后判断：id 为 `[a-z0-9_]+` 且唯一；instructions 非空；noul 的 `yes` / `no` 各自可选；
  - choice 2..=20 个选项（项目上限）；score 至少 2 档；label 与 level 非空且唯一。
- **Jev**：model 必须是 `jev-…`，别名 `jev-latest` / `jev-preview` 给 warning；score ≤ 10 档。
- **Laya**：
  - model ∈ `multilingual` / `english` / `typed-decisions`，**`auto` 拒绝**：它按输入语言选检查点，违反 non-goals §5.2；
  - 问题 ≤ 50；instructions ≤ 2000 字符；score ≤ 50 档；`max_len` 512..=8192。
- **响应**（失败即 `InvalidResponse`；原因是固定文本加问题 id，不回显 provider 内容）：
  - 顶层：
    - Jev：用版本号时 `model` 须等于请求值，用别名时只要求 `jev-` 前缀；
    - Laya：`routing.model` 必需且等于请求的检查点，`usage.truncated` 与 `usage.state_tokens_dropped` 必需；
      `usage.options` 中出现 `distinct < total`（选项塌缩）即拒绝。
  - answers：拒绝重复键；键集合与问题 id 完全相等；每个答案的 type 与问题一致。
  - 取值域：概率、`noul`、置信度为 [0,1] 内的有限值；score 在 [0, n−1] 内。
    必需字段：Jev 的 choice / score 要有 `confidence`；Laya 三种题型都要有 `confidence` 与 `answer_confidence`。
  - 分布：键集合精确匹配，|Σp − 1| ≤ n·ε + 1e-6。
    ε 为半个末位单位：Laya 5e-5（源码确认 4 位小数）；Jev 暂取 5e-3，与官方示例一致，待真机探针实测后收紧。
  - 一致性：
    - choice 的 label 必须是概率最大项之一（舍入单调，与精度无关）；
    - |score − Σi·pᵢ| ≤ ε·(n(n−1)/2 + 1) + 1e-6。
  - 复算：只复算 Laya 的 noul `confidence` = max(p,1−p) 与各题型 `answer_confidence` = max(p)，容差 2ε。
    Jev 的置信度和 Laya 的熵置信度只检查取值域。

## 4. 结果类型

`DecisionResponse { execution, answers, usage }`：

- `execution`：provider、requested_model、reported_model；Laya 另有 `routing_model`（检查点名，不是权重版本）。
- `usage`：`input_tokens`；`state` 为 `NotApplicable`（Jev）或 `Laya { truncated, state_tokens_dropped }`。
- `answers`：按问题顺序排列，每项是 `Noul { p_yes }`、`Choice { label, probabilities }` 或
  `Score { expected, probabilities }`。choice / score 另带 `spread_confidence { kind, value }`，kind 为
  `JevChoice` / `JevScore` / `LayaEntropy`；Laya 还带 `top_probability`（即 `answer_confidence` = max(p)，
  **未经校准**）。不同 kind 的阈值不可互用。

## 5. 错误

| HTTP / 情形 | `DecisionError` | retryable 提示 |
|---|---|---|
| 401 / 403 | `Unauthorized` | 否 |
| 400 / 422 | `Rejected { status, code }` | 否 |
| 413 / 415 | `PayloadTooLarge` / `UnsupportedEncoding` | 否 |
| 本地超时 | `Timeout { seconds }` | 是 |
| 408 | `RequestTimeout` | 是 |
| 429 | `RateLimited { retry_after_seconds }`：只认 `Retry-After` 的十进制秒数（`1*DIGIT`）；HTTP-date、`+7`、溢出都记为无提示 | 是 |
| 503 / 529 | `Unavailable { status, code }` | 是 |
| 其它 5xx | `Server { status }` | 是 |
| 其它非 2xx（含 3xx，不跟随） | `HttpStatus { status }` | 否 |
| 连接失败 | `Connection` | 是 |
| 响应体 > 1 MiB | `ResponseTooLarge` | 否 |
| 非 2xx 的错误体超限、读取超时或中断 | 按 status 归类（`code` 记为无） | 同该 status |
| 非 JSON / 不合契约 | `InvalidResponse` | 否 |

- `code` 只取 `detail.code`、`code`、`error.code` 或字符串形式的 `error`，且须满足 `[A-Za-z0-9_.-]{1,64}`、
  不含 key；错误体其余内容一律不回显。
- retryable 只是提示，Phase A 不重试。任何错误都只表示"没有判断"，不能推断为不重要。

## 6. 配置

字段、缺省值与示例见 [./06-config.md](./06-config.md) §typed-decision。

- **validate-config**：
  - 有 `[decision]`：按 provider 的 profile 校验各板块问题，关闭时也校验，不碰网络；
  - 无 `[decision]`：只做公共校验；空问题列表跳过；
  - 有问题但未启用：`decision_inert` warning（带 `category`）。
  - 启用时检查 `api_key_env` 所指变量是否存在：只拿到"是否存在"的结果，不读值；受 `enforce_env_checks` 控制。
- **其它命令**在 Phase A 不读这两段：段内配置有错也不影响 ingest / ai-run / publish。
- **`build_client`**：关闭时返回 `None`，不解析凭据，也不构造 client。
- **`policy_version`**：
  - 形如 `{"max_state_chars":N,"policy_schema":1,"questions":[…],"state_shape":["title","body"]}`：键按字典序，
    数组保持配置顺序，问题已规范化，choice 的 description 未填为 `null`；对该 JSON 取 SHA-256，输出小写 hex。
  - 两组 JSON / hash 测试向量见 `policy.rs`，均已用 `sha256sum` 独立核对。
  - provider、model、`laya_max_len` 不进 hash。

## 7. 测试矩阵

| 内容 | 位置 |
|---|---|
| 公共校验与规范化、两组 hash 向量、分布 / argmax / 期望值、重复键、错误分类与白名单码、Retry-After 解析、`base_url` 规则与 Debug 脱敏、配置解析与 warning、关闭时不解析凭据 | `crates/decision/src/*` 单元测试（35 个） |
| 两家请求体的确切形状（含规范化与单侧 noul 释义）；正常响应的归一化（Jev 两位小数的官方示例可通过）；拒绝：顶层元数据（Laya `routing.model` / `usage.truncated` / `usage.state_tokens_dropped` 缺失或不符、选项塌缩、缺 `model`）、Jev pin 不符、答案缺失 / 多余 / 类型不符、重复键、取值域、概率和、argmax、期望值、Laya 置信度复算、两家必填置信度、线格式数字（负数、指数 / 小数 token、超出 u64、NaN、字符串）；全部 status 类别、408、Retry-After、3xx 不跟随；错误体不回显；响应体上限；错误体停滞或中断时保留 status；超时；本地拒绝不发请求；连接被拒 | `crates/decision/tests/contract_tests.rs`（wiremock 与原始 TCP，23 个；client 关闭环境代理，在设置了失效代理时同样通过） |
| validate-config：未启用时的 warning、启用时才检查密钥变量、拒绝 Laya `auto` | `crates/cli/tests/validate_config_cmd_tests.rs` |
| 透传：缺段时不出现、有段时原样保留 | `crates/config/src/decision.rs` |
| 真机探针（`#[ignore]`）：连通性、契约、Jev 实际返回的精度 | `crates/decision/tests/live_probe.rs` |
