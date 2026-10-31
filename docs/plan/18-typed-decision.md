# 18 — typed-decision 能力（Phase A / B）

决策与一手来源见 [../adr/0010-typed-decision-capability.md](../adr/0010-typed-decision-capability.md)（Phase A，accepted）
与 [../adr/0011-decision-run-shadow-runner.md](../adr/0011-decision-run-shadow-runner.md)（Phase B，accepted）。
§1–§7 是 Phase A（能力层）的契约，§8 是 Phase B（独立的 `decision-run` 影子运行）的契约。
默认关闭；不配置 `[decision]` 时生产行为零变化，`decision-run` 也不在 `run` 中。

## 1. 边界与可移除性

- 全部实现在 [`crates/decision`](../../crates/decision/)（能力层，只依赖 domain 已有的 `SecretString` /
  `ClassifiedError`，不修改 domain）。
- 其它 crate 只有两处入口：
  - config：`LoadedConfig.decision`（[`crates/config/src/decision.rs`](../../crates/config/src/decision.rs)）原样透传
    `[decision]` 与 `[category.decision]` 为 `toml::Value`；
  - cli：validate-config 调用 `rss_ai_news_decision::validate_sections`。
- 整体移除：
  1. 删 `crates/decision`、workspace 条目与 `crates/cli/Cargo.toml` 中的依赖；
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
    ε 为半个末位单位：Laya 5e-5（源码确认 4 位小数）；Jev 5e-3（官方示例与 2026-10-31 真机探针均为 2 位小数）。
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
| 公共校验与规范化、两组 hash 向量、分布 / argmax（含并列）/ 期望值、概率重复键、错误分类与白名单码、Retry-After 解析、`base_url` 规则与 Debug 脱敏、配置解析与 warning、配置的类型错误不回显输入值、关闭时不解析凭据 | `crates/decision/src/*` 单元测试（36 个） |
| 两家请求体的完整 JSON（含规范化与单侧 noul 释义）；正常响应的归一化（Jev 两位小数的官方示例可通过）；拒绝：顶层元数据（Laya `routing.model` / `usage.truncated` / `usage.state_tokens_dropped` 缺失或不符、选项塌缩、缺 `model`）、Jev pin 不符、答案缺失 / 多余 / 类型不符、答案重复键、概率越界、置信度越界（Laya 六处、Jev 三处）、概率和、argmax、期望值、Laya 复算（三种题型的 `answer_confidence`、noul 的 `confidence`）、必填置信度（Jev choice / score，Laya 六处）、线格式数字（负数、指数 / 小数 token、超出 u64、NaN、字符串）；全部 status 类别、408、Retry-After、3xx 不跟随；错误体不回显；响应体上限（Content-Length 预检；无 Content-Length 时的流式上限未单独构造用例）；错误体停滞或中断时保留 status；超时；本地拒绝不发请求；连接被拒 | `crates/decision/tests/contract_tests.rs`（wiremock 与原始 TCP，25 个；client 关闭环境代理，在设置了失效代理时同样通过） |
| validate-config：未启用时的 warning、启用时才检查密钥变量、拒绝 Laya `auto` | `crates/cli/tests/validate_config_cmd_tests.rs` |
| 透传：缺段时不出现、有段时原样保留、Debug 不打印原始值 | `crates/config/src/decision.rs` |
| 真机探针（`#[ignore]`）：连通性、契约、Jev 实际返回的精度 | `crates/decision/tests/live_probe.rs` |

覆盖口径是"每条拒绝规则至少一个反例"，不是每个分支组合都有用例。

## 8. decision-run（Phase B）

设计与取舍见 ADR 0011；本节记录实现契约与偏离。

### 8.1 位置

| 层 | 文件 |
|---|---|
| CLI | `crates/cli/src/commands/decision_run.rs`（`Command::DecisionRun`，`--max-requests` / `--window-hours`） |
| flow | `crates/runtime/src/flows/decision_run/{mod,task}.rs`（`DecisionRunDeps` 在模块内） |
| storage | `crates/storage/src/repo/decision_result{,_sql,_impl}.rs`，迁移 `0006_decision_results` |
| 能力 | `crates/decision`：`Exchange` / `RawInfo` / `SentState`、`answers_document`、`revalidate`、`questions_from_canonical` |

移除步骤见 ADR 0011 §13（先停调度、确认进程退出，再发布带删表迁移的版本）。

### 8.2 客户端扩展

- `DecisionClient::decide(request, deadline) -> Exchange`：单次请求的超时取配置值与剩余时间的较小者；
  已收到的非 2xx 状态在读体超时或中断时仍按该状态归类。
- `Exchange.sent`：`state_sha256`（`state` 紧凑 JSON、键排序后的 SHA-256）、截断后正文字符数、是否截断；请求未构造时为 `None`。
- `Exchange.raw`（仅 2xx）：`bytes`、完整体的 `sha256`、可安全保存时的 `body`，否则 `withheld` 为
  `not_json` / `credential` / `incomplete`。
- 凭据检查：响应体按字节查找 key，并对每个 JSON 字符串字面量（含对象键、重复键）解码转义后查找；
  `reported_model` / `routing_model` 含 key 时整条响应判为 `InvalidResponse`。
- 错误按阶段映射：构造失败 → `InvalidConfig`；超时 → `Timeout`；连接阶段 → `Connection`（未发出）；
  其余 → 新增的 `Transport`（可能已发出）。key 在构造客户端时验证为合法 header 值；reqwest 自动重试关闭。
- 项目上限对两家统一：每板块 ≤ 50 题、每题 instructions ≤ 2000 字符。

### 8.3 数据

- `decision_policies(policy_version, canonical_json)`：每次运行为参与的板块写入当前策略快照（已存在即跳过）。
- `decision_results`：一行 = 文章 × `policy_version` × `execution_key`（`jev:{model}` / `laya:{model}:{laya_max_len}`）；
  列与约束见迁移 0006 与 ADR 0011 §3。状态机 `DecisionResultState` 见 plan/08 §1。
- `answers_json`（`answers_document`，`schema = 1`）示例：

  ```json
  {"answers":[{"id":"q_ad","p_yes":0.25,"top_probability":0.75,"type":"noul"},
              {"id":"q_kind","label":"paper","probabilities":{"other":0.25,"paper":0.75},
               "spread_confidence":{"kind":"jev_choice","value":0.5},"top_probability":null,"type":"choice"}],
   "schema":1}
  ```

- 原始响应：`raw_response = on_failure`（缺省，只保存未通过校验的 2xx）| `always` | `never`；长度、摘要与
  未保存原因总是记录。保存时写 `raw_response_expires_at`（`ttl_days = 0` 为永久）。

### 8.4 运行

- 顺序：留档清理（跨身份，每次运行上限 `max(1000, 2 × 预算)` 行，达到上限时摘要标记 `raw_gc_backlog`，
  表示可能仍有过期留档）→ 策略快照 → 每个身份的 reclaim 与预算终态化 → 板块轮转。
- 每一步领取一个窗口（≤ `concurrency`，且不超过剩余预算），全部派发、全部收取后再进入下一步；
  没有可领取的行时才生成，每次最多一个窗口。
- **偏离 ADR 0011 §5 的实现细节**：生成是单条 `INSERT … SELECT … ON CONFLICT DO NOTHING`（带窗口、`retired` 过滤与反连接），
  没有分页游标；`NOT EXISTS` 排除已有行，并发冲突只会让本次插入数变少，不会卡住。
- 单行：核对身份 → 读文章（存储期限 5 s）→ 最后一次 lease 检查（剩余 ≥ 请求超时 + 5 s，否则退还并暂停该板块）
  → 调用（计一次额度）→ 按 ADR 0011 §10 的类别释放。
- 收尾：窗口在 `lease_seconds` 内未收完即 `abort_all`，再收取 5 s，未确认的行留给 lease 回收，计 `drain_timeout`。
- 事件：stage `decision_run`，由协调者在每个窗口后写，不含正文、答案与响应体。

### 8.5 测试

| 内容 | 位置 |
|---|---|
| 客户端：`Exchange` 四种原始体形态、凭据反射（含转义与重复键）、`Transport` / `Connection`、截止时间与已知 status、不自动重试、非法 header key、`revalidate` 与在线一致 | `crates/decision/tests/contract_tests.rs` |
| `answers_document` 固定形状、策略 JSON 往返、项目上限 | `crates/decision/src/*` 单元测试 |
| 存储：窗口（含亚秒与 SQLite 文本格式）、身份隔离、并行领取不相交、三种释放、维护、留档清理、约束 | `crates/storage/tests/decision_result_tests.rs`；PostgreSQL 版 `decision_result_pg_tests.rs`（postgres lane） |
| flow：成功与事件、预算上限、全局 / 单篇失败、限流、疑似配置、留档策略、首轮公平、策略切换、lease 不足、panic、收尾超时 | `crates/runtime/tests/decision_run_tests.rs` |
| CLI：配置类错误 78 且不开库、首轮预算、`--dry-run` 拒绝、对 mock laya-server 的端到端 | `crates/cli/tests/decision_run_cmd_tests.rs` |
