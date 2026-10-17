# ADR 0010: typed-decision 能力（Jev / Laya）—— 独立能力层与分阶段接入

- 日期：2026-10-17
- 状态：`proposed`
- 决策者：Develata（方向已定：两个 provider、问题按板块配置、shadow 走独立 `decision-run`）；细节待本 ADR 批准
- 评审：Codex（gpt-6-astra，medium）两轮。首稿 3 项 BLOCKER、9 项 SHOULD_FIX；二稿 1 项 BLOCKER（置信度复算与 Jev 官方示例不兼容）、
  5 项 SHOULD_FIX。均已对照源码与官方示例核实，并在本稿处理

## Context

现有 AI 链路是生成式 LLM（OpenAI Chat Completions 兼容）：一次调用产出 `keep_decision` / `summary` /
`tags` / `importance_score`。用户希望引入 typed-decision 模型（TypeSafe Jev 托管 API、自部署 laya-server），
对文章回答固定类型的问题（是 / 否、单选、分档），用于后续评估能否辅助过滤或排序。

硬约束：

- **摘要不能丢**：typed-decision 不生成文本，只能辅助；现有生成链路、摘要 parser、`importance_score`
  语义、发布状态机都不变。
- 默认关闭；关闭时不解析、不使用新凭据，不构造 client，不发请求（精确定义见 §6）。
- 不把 provider 原始 JSON 透传进核心层；provider 差异在适配层消化。
- non-goals §5.2（不做按输入的智能模型路由）、§8（永远 Rust；调用外部 Python 服务的 HTTP API 不改变这一点）、
  宪法 §3.5（平级模块优于改写主流程）。

两个 provider 同为 `POST /v1/systemone`，契约不同。一手来源：

- docs.typesafe.ai 的 /api、/models、/confidence、/primitives/score；
- laya-server `434e9b3` 的 `backend/server/{schemas,main,laya_adapter}.py`；
- 上游 Laya `6d942c9` 的 `laya/{agent,common,confidence,router}.py`。

| 维度 | Jev（托管，文档 2026-10-17） | laya-server `434e9b3` + Laya `6d942c9`（用户所称 v0.1.2；实际部署镜像与权重未核实） |
|---|---|---|
| 鉴权 | `Authorization: Bearer` | `Authorization: Bearer`（服务端签发 key） |
| `model` | 必填；版本号 `jev-1.13.0` / `jev-1.14.0`，别名 `jev-latest` / `jev-preview` 会移动 | 服务端接受 `auto` / `english` / `multilingual` / `typed-decisions`；multilingual-only 镜像只接受 `auto` / `multilingual`（否则 422 `MODEL_NOT_AVAILABLE`）；`auto` 在非 multilingual-only 部署中**按输入语言选检查点** |
| 请求字段 | `state`、`model`、`questions`；未知字段处理未写明 | 只允许 `state` / `questions` / `model` / `max_len`（`extra="forbid"`） |
| `instructions` | string / object / array | 字符串，trim 后 1..=2000 字符 |
| choice 选项 | map，≤ 255 | map 或 list，2..=50；label trim 后非空且唯一 |
| score 档位 | 有序数组 2..=10，可为对象 | 字符串数组 2..=50；trim 后非空且唯一 |
| 问题数 | 无独立数量上限（受 token 预算约束） | 1..=50 |
| 长度 | 默认 64k token / 请求，**且** 32k token / 单题（state + 该题）；超出 400 `max_tokens_exceeded`；`TypeSafe-Danger-Zone-Long-Context` 头可提至 262k（本项目不用） | `max_len` 是**每道题**的编码序列预算（512..=8192）；不传时取检查点配置（代码兜底 512）；state 超出部分被截断（`usage.truncated`、`state_tokens_dropped`）；选项超出 head 预算时塌缩为不可区分（`usage.options`）；选项标记完全放不下时抛异常，服务端返回 503 |
| noul 答案 | `noul`（是的概率），**无** `confidence` | `noul`、`confidence` = max(p,1−p)、`answer_confidence` |
| choice / score 的 `confidence` | choice：`(n·p_max−1)/(n−1)`；score：`max(0, 1 − Σpᵢ·\|i−m\| / D_n)`，m 为首个最大概率档位，`D_n = (1/n)·Σ\|i−(n−1)/2\|`；返回值精度未写明（官方示例与公式复算相差 0.005） | 1 − 归一化熵（由舍入前概率计算后再四舍五入到 4 位） |
| `answer_confidence` | 无 | `max(p)`；上游注明出厂检查点过度自信，multilingual **未拟合温度**，**不是**已校准概率 |
| 顶层 `model` | 实际回答的版本号，如 `jev-1.13.0` | 恒为 `laya-rl-agent`；实际检查点名在 `routing.model`（检查点名不是权重 revision） |
| 错误 | 400 / 401 / 413 / 415 / 422 / 429 / 529；错误体为"描述原因的 JSON"，结构未写明 | 401 / 422（`{"detail":{"code":…}}`，校验失败固定 `VALIDATION_ERROR`）/ 503（推理异常——含输入导致的确定性失败——以及用量写库失败均为 503）；其它异常可能为 500 |
| 问题 id | 文档写明不送入模型 | 默认编码路径不送入模型（`agent.py:852` 只用 type / instructions / criteria），未实测 |
| 语言 | 官方称英语最佳，CJK 可用但准确度较低 | 上游声称支持 100+ 语言；本项目板块上的质量需 Phase C 验证 |

## Decision

### 1. 骨架位置

- 新增**能力层** crate `crates/decision`（`rss-ai-news-decision`），与 `crates/ai` 平级，只依赖 domain。
- 归一化类型与**纯数据校验**放进 domain（`dto::decision`）：问题规格、provider 限制
  （`ProviderLimits::validate_questions`）、规范化与 `policy_version`。
  config 与 decision 都调用 domain 的同一份实现，config 不依赖 decision crate。
- provider 线格式 DTO、HTTP、响应校验留在 decision crate 内部，不外泄。
- `read_limited_body` 与 key 脱敏两个小函数在 decision crate 内复制一份，并注明出处
  `crates/ai/src/client.rs`；不为约 40 行代码耦合 ai crate。
- 不改 `crates/ai`、不改 ai-run / publish 流程、不改现有表、不加 CLI 子命令。

### 2. 接口与归一化类型

```rust
#[async_trait]
pub trait DecisionClient: Send + Sync {
    async fn decide(&self, request: &DecisionRequest) -> Result<DecisionResponse, DecisionError>;
}
```

- `DecisionRequest { state: { title, body }, questions: Vec<QuestionSpec> }`：body 先按 `max_state_chars`
  （Unicode 字符）截断。这只是预截断，不保证满足 token 限制：Jev 超限会返回 400，Laya 会截断并报告。
  它与生成模型的 `max_input_chars` 互不影响。
- `QuestionSpec { id, instructions, kind }`，`kind` 为：
  - `Noul { yes: Option<String>, no: Option<String> }`：映射到 `criteria.true` / `criteria.false`，两者都没有时省略 `criteria`；
  - `Choice { options: Vec<{ label, description: Option<String> }> }`：发送为 map，label 不允许重复覆盖；description 为 None 时，Jev 发 `null`，Laya 发空串；
  - `Score { levels: Vec<String> }`。
- `DecisionResponse`：
  - `execution`：`provider`、`requested_model`、`reported_model`；Laya 另带 `routing_model`（必需）；
  - `answers: Vec<(id, DecisionAnswer)>`：顺序与问题一致；
  - `usage`：`input_tokens`；Laya 另带 `state_truncated`、`state_tokens_dropped`（必需），Jev 为 `NotApplicable`。

  "不适用"与"应有但缺失"区分开：后者是 `InvalidResponse`。
- `DecisionAnswer`：
  - `Noul { p_yes }`
  - `Choice { label, probabilities }`
  - `Score { expected, probabilities }`

  三种都附带：
  - `spread_confidence: Option<{ kind: JevChoice | JevScore | LayaEntropy, value }>`：只用于 choice / score；noul 两家都取 None，可由 `p_yes` 推出。
  - `top_probability: Option<f64>`：取 Laya 的 `answer_confidence`，即 `max(p)`。**命名刻意不叫 calibrated**：它未经本项目数据校准，阈值必须在 Phase C 用留出数据重新拟合。

### 3. provider profile（请求前校验，在 domain 内实现）

- **公共**：
  - 问题 id 唯一、非空、只含 `[a-z0-9_]`，用中性命名（如 `q_marketing`）；
  - instructions trim 后非空；
  - choice 2..=20 个选项：项目限制。Jev 允许 1..=255；Laya 约 20 个选项后会超出 head 预算而塌缩，
    上限 20 同时让概率容差保持有意义；
  - label 与 level 都 trim 后判空、判重。
- **Jev**：
  - `model` 必填；使用别名时 validate-config 给 warning（答案会随别名移动）；
  - choice ≤ 255；score 档位 2..=10；不发 `max_len`。
- **Laya**：
  - `model` 只允许**显式检查点名**（`multilingual` / `english` / `typed-decisions`），**不允许 `auto`**。
    `auto` 在非 multilingual-only 部署中按输入语言选检查点，违反 non-goals §5.2。示例配置用 `multilingual`。
  - instructions trim 后 ≤ 2000 个 Unicode 字符（与 Python `str` 长度一致，不是字节）；
  - choice / score 2..=50；问题数 1..=50；
  - **总是显式发送** `max_len`（512..=8192，不依赖服务端缺省）；请求体只含允许的四个字段。

### 4. 响应校验（不通过即 `InvalidResponse`，绝不当作"不重要"）

- **顶层**：
  - `model` 为非空字符串；
  - Jev 用版本号时，`reported_model` 必须等于请求值；用别名时只要求 `jev-` 前缀；
  - Laya 的 `routing.model` 必须等于请求的检查点名；
  - `usage.input_tokens` 为非负整数；Laya 必须有 `truncated`（bool）与 `state_tokens_dropped`（非负整数）。
- **answers**：
  - JSON 对象的**重复键直接拒绝**：自定义反序列化，不先装进普通 map；
  - 键集合与问题 id 完全相等；
  - 每个答案的 `type` 与问题类型一致。
- **取值域（独立于一致性检查，类型不变量）**：
  - 所有概率、`p_yes`、置信度均为有限值且 ∈ [0,1]；score 的 `expected` 为有限值且 ∈ [0, n−1]。
  - 必需字段：Jev 的 choice / score 必须有 `confidence`；Laya 三种题型必须有 `confidence` 与 `answer_confidence`。
    归一化 DTO 中的 `Option` 只表示"该 provider 不适用"，不放宽线格式必填。
- **分布**：键集合精确匹配（choice 为选项 label；score 为 `"0"..="n-1"`），拒绝重复键。
- **一致性**（容差取决于 provider 的返回精度 ε，即半个末位单位）：
  - ε_Laya = 5e-5（源码确认四舍五入到 4 位）。
  - ε_Jev 暂取 5e-3，与官方示例的 2 位小数一致；Phase A 的真机探针会记录实测精度，再据此收紧并写回本节。
    确认之前不会用更严的界拒绝 Jev 响应。
  - 概率和：|Σpᵢ − 1| ≤ n·ε + 1e-6。
  - choice：`label` 必须是概率 argmax 之一（并列时任一最大项都合法）。舍入单调不减，真实 argmax 舍入后仍是最大项之一，
    所以这条与精度无关。
  - score：|expected − Σi·pᵢ| ≤ ε·(n(n−1)/2 + 1) + 1e-6。
- **置信度复算**：只对可由返回分布精确推导、且误差界已证明的量做：
  - Laya noul：|confidence − max(p_yes, 1−p_yes)| ≤ 2ε；
  - Laya 三种题型：|answer_confidence − max(p)| ≤ 2ε（max 为 1-Lipschitz）。
  - Jev 的 choice / score `confidence`（精度未知，score 的众数在并列附近会跳变）与 Laya 的熵 `confidence`
    （非线性，50 个选项时合法响应的复算偏差可达约 0.007）**不复算**，只做取值域与必填检查，原值保存为 provider 统计量。
- **Laya 输入保真**：
  - `usage.options` 中任一问题 `distinct < total`（选项塌缩为不可区分）→ 拒绝该响应，`InvalidResponse { reason: option_collapse }`；
  - state 截断不拒绝，但必须如实记录。
- 未知的额外字段忽略。`legend` 不回显、不信任。响应体上限 1 MiB；错误消息先脱敏再截断。

### 5. 错误语义（闭合分类）

`DecisionError` 实现 `ClassifiedError`，所有非 2xx 都有归属，并保留 HTTP status：

| 情形 | 变体 | retryable 提示 |
|---|---|---|
| 401 / 403 | `Unauthorized` | 否 |
| 400 / 422 | `Rejected { status, code: Option }`：宽松解析 Laya 的 `detail.code` 或 Jev 的 `code` / `error` | 否 |
| 413 | `PayloadTooLarge` | 否 |
| 415 | `UnsupportedEncoding`（本项目不压缩请求，出现即实现缺陷） | 否 |
| 408 / 超时 | `Timeout` | 是 |
| 429 | `RateLimited { retry_after }` | 是 |
| 503 / 529 | `Unavailable { status, code }`（Laya 的 503 也可能来自输入导致的确定性失败） | 是（仅提示） |
| 其它 5xx | `Server { status }` | 是（仅提示） |
| 其它非 2xx | `HttpStatus { status }` | 否 |
| 连接失败 | `Connection` | 是 |
| 响应体超限 / 非 JSON / 校验失败 | `ResponseTooLarge` / `InvalidResponse { reason }` | 否 |
| 配置错误 | `InvalidConfig` | 否 |

- 错误体超限或不是 JSON 时，status 语义照常保留。
- `retryable` 只是提示。Phase A 不做任何自动重试；重试上限与退避由 Phase B 的 `decision-run` 决定。
- 任何错误都只表示"没有判断"，调用方不得据此推断文章价值。

### 6. 配置（用户已定：问题按板块）

```toml
# app.toml —— 整段可省略
[decision]
enabled = false
provider = "laya"                      # 有此段即必填："laya" | "jev"
base_url = "http://laya.internal:8080" # enabled = true 时必填；Jev: https://api.typesafe.ai
api_key_env = "LAYA_API_KEY"           # enabled = true 时必填；只存 env 变量名，key 不进 toml
model = "multilingual"                 # enabled = true 时必填，无内置缺省；Laya: multilingual|english|typed-decisions；Jev: jev-1.13.0
request_timeout_seconds = 10           # 缺省 10，1..=60
max_state_chars = 6000                 # 缺省 6000，1..=200000
laya_max_len = 2048                    # 仅 Laya；缺省 2048，512..=8192；provider = "jev" 时设置即 warning（不生效）

# categories/<key>.toml —— 可省略；省略或空列表 = 该板块不做 typed-decision
[[category.decision.questions]]
id = "q_marketing"
type = "noul"
instructions = "这篇是否主要是厂商营销或公关稿？"

[[category.decision.questions]]
id = "q_importance"
type = "score"
instructions = "这条新闻对该领域读者的重要性？"
levels = ["琐碎", "一般", "值得关注", "重大"]

[[category.decision.questions]]
id = "q_kind"
type = "choice"
instructions = "这篇属于哪一类？"
options = [{ label = "release", description = "新模型或新版本发布" },
           { label = "paper", description = "研究论文" },
           { label = "other" }]
```

- `[decision]` 用 `Option<DecisionConfig>` 表示，以区分"缺段"与"有段但关闭"；板块问题同时加入 `RawCategorySection`
  与手写的 `Deserialize` 转换，并为旧配置、缺段、空段、关闭、有问题无全局段分别写测试。
- 不含 `concurrency`：并发属于 Phase B，届时需说明与 non-goals §1.2 的关系（单实例内的有界并发，同 extract / ai-run）。
- **validate-config 规则**：
  - 空问题列表表示该板块不调用，跳过问题数下限检查；
  - 有 `[decision]` 段：按 `provider` 对应的 profile 校验每个非空板块的问题，enabled = false 也校验，不碰网络；
  - 无 `[decision]` 段：只做公共校验；
  - 板块写了问题、但 `[decision]` 缺失或 enabled = false：非致命 warning（code `decision_inert`，带 `category`）；
  - Jev 用别名、或 provider = jev 却设了 `laya_max_len`：warning。
- **"关闭时不访问新凭据"的精确定义**：
  - 现有 loader 在读 `app.toml` 之前就会用 `dotenvy::from_path_iter` 逐项解析整个 `.env`，任一条目语法错误都会使加载失败。
    这对所有键一直如此，本特性不改变它。
  - 本特性保证：`enabled = false` 时，不会因 decision 额外调用 `resolve_secret`、检查凭据是否存在或使用凭据；
    不构造 client，也不建立任何连接。
  - `enabled = true` 时，凭据检查受现有的 `enforce_env_checks` 控制（`load_skip_env_checks` 路径照旧跳过）。
    Phase A 中只有 validate-config 会触发该检查。
  - 检查沿用 `resolve_secret` 的现有语义：进程环境里有这个变量就用它，即使值为空白也不再回退到 `.env`；
    最终值为空白视为缺失。只报变量名，不回显其值。
- **`policy_version`**：对规范化 JSON 的 UTF-8 字节取 SHA-256，输出 64 位小写 hex。规范化规则：
  - 先对所有字符串 trim；
  - 对象键按字典序排列，数组保持配置顺序，无空白；转义用 serde_json 的紧凑输出；
  - 内容为 `{"max_state_chars":N,"policy_schema":1,"questions":[…],"state_shape":["title","body"]}`；
  - 每个问题含 `id`、`type`、`instructions`，再按题型加：
    - noul：`yes` / `no`，未填即省略；
    - choice：`options`，即 `[{"label":…,"description":…}]`，description 未填为 `null`；
    - score：`levels`。

  Phase A 提供至少两组固定的 JSON 与 hash 测试向量，覆盖三种题型、可选值和中文。

  provider、model、`laya_max_len` 属于执行参数，不进 hash，由 Phase B 与结果一起记录（含 `state_truncated`）。

### 7. 分阶段

- **Phase A（本 ADR 批准后实施）**：
  - 交付：decision crate（trait、SystemOne HTTP 实现、两个 profile、§4 全部响应校验、§5 错误）、domain DTO 与纯校验、
    config 解析 / 校验 / warning；
  - 离线契约测试（wiremock），覆盖：
    - 请求体的确切形状：Laya 只有四个字段，且总带 `max_len`；
    - 两家正常响应的 fixture；
    - §4 的每条拒绝规则：分布不一致、置信度不一致、重复键、缺失元数据、选项塌缩；
    - §5 的每类 status 与超大、非 JSON 错误体；超时；
    - 脱敏；旧配置兼容；关闭时零客户端、零请求；
  - 一个 `#[ignore]` 的真机探针：用 env 显式选择 provider，Jev / Laya 各发 1 个小请求，只验证连通与契约，不作为质量证据；
  - 不接入任何 flow，不建表，不加 CLI 子命令，不做自动重试、路由或置信度过滤。
- **Phase B（另立 ADR）**：独立 `decision-run` 子命令。
  - 自有表：article_id、policy_version、执行参数、state 指纹、截断信息、校验后的答案、error_kind、latency、tokens，
    以及部署标识（如 base_url 主机与 reported / routing model）；
  - 自有 claim / lease、有界并发、超时、重试上限与预算，双方言迁移；
  - 只读文章，不写 `article_ai_results` / `articles.state`，不覆盖 `ai_raw_response`；
  - 关闭时连接都不建立。
- **Phase C（另行决定）**：人工标注评估集与指标，包括阈值拟合；是否让判断参与过滤属于骨架级决策，届时单独提交。

## Consequences

### 正面后果
- provider 差异集中在 domain 的纯校验与 decision crate 的适配中，核心层只见到通过一致性校验的类型化结果；
  新增或替换 provider 不改核心。
- 默认关闭、Phase A 不接入流程：生产行为零变化。
- 置信度带来源标签且不冒充"已校准"，避免把 Jev 的阈值套到 Laya 上。

### 负面后果 / 代价
- 新增一个 workspace crate 与一组配置段：公开配置契约扩大，需要承担版本责任。
- Jev 官方自称对中文的准确度较低；Laya 的 `max_len` 是每题预算，长文章会截断。两者都要在 Phase C
  用真实数据验证，Phase A 只保证契约正确。
- 真机探针依赖用户提供的 Jev key 与可达的 Laya 实例，不进 CI。
- 数据外发：
  - Jev 为托管服务，启用后文章标题与截断后的正文会发往 TypeSafe，与现在发往 OpenAI 兼容端点属于同一类外发；
  - Laya 的数据去向取决于 `base_url` 与部署网络（自部署不等于不出网），由部署方核实。
- 费用：Jev 按输入 token 计费（文档：$0.042 / 百万 token，输出免费），以账单为准。

### 后续行动
- 批准后实施 Phase A；同步 plan/18（契约全文）、plan/06（配置）、plan/13（说明 typed-decision 不是模型路由）、地图。
- Phase B 设计（decision-run、表结构、lease 预算、部署标识）单独提交 ADR。

## Links

- [plan/18-typed-decision.md](../plan/18-typed-decision.md)（待建）
- [plan/13-non-goals.md](../plan/13-non-goals.md) §1.2、§5、§8
- Jev：https://docs.typesafe.ai/api 、/models 、/confidence 、/primitives/score
- laya-server：https://github.com/1Panel-dev/laya-server/tree/434e9b350f29a88afd773aee99ca01ee32fffb24
- Laya：https://github.com/NandhaKishorM/laya/tree/6d942c92081fbc139e736bbd9ac0023223c29b7f
