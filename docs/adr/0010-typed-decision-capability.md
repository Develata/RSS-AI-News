# ADR 0010: typed-decision 能力（Jev / Laya）—— 独立能力层与分阶段接入

- 日期：2026-10-17
- 状态：`proposed`
- 决策者：Develata（方向已定：两个 provider、问题按板块配置、shadow 走独立 `decision-run`）；细节待本 ADR 批准

## Context

现有 AI 链路是生成式 LLM（OpenAI Chat Completions 兼容）：一次调用产出 `keep_decision` / `summary` /
`tags` / `importance_score`。用户希望引入 typed-decision 模型（TypeSafe Jev 托管 API、自部署 Laya），
对文章回答固定类型的问题（是 / 否、单选、分档），用于后续评估是否能辅助过滤或排序。

硬约束：

- **摘要不能丢**：typed-decision 不生成文本，只能辅助；现有生成链路、摘要 parser、`importance_score`
  语义、发布状态机都不变。
- 默认关闭；关闭时不读取新凭据、不发额外请求。
- 不把 provider 原始 JSON 透传进核心层；provider 差异在适配层消化。
- non-goals §5.2（不做按输入的智能模型路由）、§8（永远 Rust）、宪法 §3.5（平级模块优于改写主流程）。

两个 provider 同为 `POST /v1/systemone`，但契约不同（一手来源：docs.typesafe.ai/api、/models、
/confidence；laya-server `434e9b3` 的 `backend/server/{schemas,main}.py`；上游 Laya `6d942c9` 的
`laya/agent.py`）：

| 维度 | Jev（托管） | laya-server v0.1.2（自部署） |
|---|---|---|
| 鉴权 | `Authorization: Bearer` | `Authorization: Bearer`（服务端签发 key） |
| `model` | 必填；版本号 `jev-1.13.0` / `jev-1.14.0`，别名 `jev-latest` / `jev-preview` 会移动 | `auto` / `english` / `multilingual` / `typed-decisions`；multilingual 镜像只接受 `auto` / `multilingual`（否则 422 `MODEL_NOT_AVAILABLE`） |
| 请求字段 | `state`、`model`、`questions`；未知字段处理未写明 | 只允许 `state` / `questions` / `model` / `max_len`（`extra="forbid"`） |
| `instructions` | string / object / array | 字符串，trim 后 1..=2000 字符 |
| choice 选项 | map，≤ 255 | map 或 list，2..=50，非空且去重 |
| score 档位 | 有序数组 2..=10，可为对象 | 字符串数组 2..=50，非空且去重 |
| 问题数 | 未写明 | 1..=50 |
| 长度 | 64k token / 请求（超出 400 `max_tokens_exceeded`） | `max_len` 512..=8192，缺省 1024 token，超出**静默截断**，`usage.truncated` 标记 |
| noul 答案 | `noul`（是的概率），**无** `confidence` | `noul`、`confidence`=max(p,1−p)、`answer_confidence` |
| choice / score 的 `confidence` | `(n·p_max−1)/(n−1)` / 基于档位离散度 | 1 − 归一化熵；校准值另在 `answer_confidence` |
| 错误 | 400 / 401 / 413 / 415 / 422 / 429 / 529；错误体为"描述原因的 JSON"，结构未写明 | 401 / 422（`{"detail":{"code":…}}`，校验失败固定 `VALIDATION_ERROR`）/ 503（推理任何异常、用量写库失败均为 503） |
| 顶层 `model` | 实际回答的版本号，如 `jev-1.13.0` | 恒为 `laya-rl-agent`；实际检查点在 `routing.model` |
| 问题 id | 文档写明不送入模型 | 源码推断不送入模型（`agent.py:852` 只用 type / instructions / criteria 构造输入），未实测 |
| 语言 | 英语最佳，CJK 可用但准确度较低 | multilingual 检查点，100+ 语言 |

## Decision

### 1. 骨架位置

- 新增**能力层** crate `crates/decision`（`rss-ai-news-decision`），与 `crates/ai` 平级；只依赖 domain，
  复用 reqwest（rustls）、serde、响应体上限与 key 脱敏的既有做法。
- 归一化后的类型放进 **domain**（`dto::decision`），作为跨 crate 契约；provider 线格式 DTO 留在
  decision crate 内部，不外泄。
- 不改 `crates/ai`、不改 ai-run / publish 流程、不改现有表。

### 2. 接口

```rust
#[async_trait]
pub trait DecisionClient: Send + Sync {
    async fn decide(&self, request: &DecisionRequest) -> Result<DecisionResponse, DecisionError>;
}
```

- `DecisionRequest`：`state { title, body }`（body 按 `max_state_chars` 截断，与生成模型的
  `max_input_chars` 互不影响）+ `Vec<QuestionSpec>`。
- `QuestionSpec { id, instructions, kind }`，`kind` 为 `Noul { yes, no }`（可选释义）、
  `Choice { options: Vec<{label, description}> }`、`Score { levels: Vec<String> }`。
- `DecisionResponse { reported_model, routing_model: Option<String>, answers: Map<id, DecisionAnswer>, usage { input_tokens, truncated: Option<bool> } }`；
  `routing_model` 取 Laya 的 `routing.model`（Jev 为 None），否则无法区分 multilingual / english 检查点。
- `DecisionAnswer`：
  - `Noul { p_yes }`
  - `Choice { label, probabilities }`
  - `Score { expected, probabilities }`，全部经过校验
- 置信度带来源标签，不混用：
  - `spread_confidence: Option<{ kind: JevSpread | LayaEntropy, value }>`：只用于 choice / score。noul 两家都取
    None：Jev 本就没有，Laya 的 noul `confidence` 是 max(p,1−p)，可由 `p_yes` 推出，不是熵。
  - `calibrated_confidence: Option<f64>`：仅 Laya 的 `answer_confidence`（三种题型都有）。

### 3. provider profile（请求前校验，不发请求就能拒绝）

同一个 `SystemOneClient`，按 `Provider::{Jev, Laya}` 选择 profile：

- 公共：问题 id 唯一、非空、只含 `[a-z0-9_]`；instructions 非空。id 用中性命名（如 `q_marketing`），
  不带倾向措辞：Jev 文档写明 id 不进模型，Laya 只是源码推断，未实测。
- Jev：`model` 必填，使用别名时 validate-config 给 warning（答案会随别名移动）；choice ≤ 255；
  score 档位 2..=10；不发 `max_len`。
- Laya：`model` ∈ 配置允许的集合；instructions trim 后 ≤ 2000 个 Unicode 字符（与 Python `str` 长度一致，
  不是字节）；choice / score 2..=50 且去重；问题数
  1..=50；`max_len` 512..=8192；请求体只含允许的四个字段。

### 4. 响应校验（不通过即 `InvalidResponse`，绝不当作"不重要"）

- answers 的 key 必须与问题 id 完全一致，类型与问题一致。
- `p_yes`、概率与置信度为有限值且在 [0,1] 内。
- choice 的 label ∈ 选项，概率键集合等于选项集合，和在 1 ± 0.01 以内（两家都四舍五入到 4 位小数）。
- score 的期望值在 [0, n−1] 内，概率键为 `"0".."n-1"`。
- 未知额外字段忽略。`legend` 不回显、不信任。
- 响应体上限 1 MiB；错误消息先脱敏再截断。`read_limited_body` 与 key 脱敏这两个小函数在 decision crate 内
  复制一份，并注明出处 `crates/ai/src/client.rs`。不抽公共模块：decision 只依赖 domain，不为约 40 行代码去
  耦合 ai crate。

### 5. 错误语义

`DecisionError` 实现 `ClassifiedError`：

- `Unauthorized`(401)
- `Rejected { code: Option<String> }`(400 / 422)：宽松解析，`detail.code`（Laya）或顶层 `code` / `error` 字段（Jev 结构未写明）
  能取到就记，取不到为 None；例如 `MODEL_NOT_AVAILABLE`、`VALIDATION_ERROR`、`max_tokens_exceeded`
- `PayloadTooLarge`(413)
- `RateLimited { retry_after }`(429)
- `Overloaded`(529)
- `Unavailable { code }`(503)
- `Timeout`、`Connection`、`ResponseTooLarge`、`InvalidResponse`、`InvalidConfig`

其中 retryable 的是：429 / 529 / 503 / Timeout / Connection。任何错误都只是"没有判断"，调用方不得据此推断文章价值。

### 6. 配置（用户已定：问题按板块）

```toml
# app.toml —— 整段可省略，缺省 enabled = false
[decision]
enabled = false
provider = "laya"                      # "laya" | "jev"
base_url = "http://laya.internal:8080" # Jev: https://api.typesafe.ai
api_key_env = "LAYA_API_KEY"           # 只存 env 变量名，key 不进 toml
model = "multilingual"                 # Laya: auto|multilingual；Jev: 固定版本号如 jev-1.13.0
request_timeout_seconds = 10
max_state_chars = 6000
laya_max_len = 2048                    # 仅 Laya，512..=8192
concurrency = 2

# categories/<key>.toml —— 可省略；省略即该板块不做 typed-decision
[[category.decision.questions]]
id = "is_marketing"
type = "noul"
instructions = "这篇是否主要是厂商营销或公关稿？"

[[category.decision.questions]]
id = "importance"
type = "score"
instructions = "这条新闻对该领域读者的重要性？"
levels = ["琐碎", "一般", "值得关注", "重大"]

[[category.decision.questions]]
id = "kind"
type = "choice"
instructions = "这篇属于哪一类？"
options = [{ label = "release", description = "新模型或新版本发布" },
           { label = "paper", description = "研究论文" },
           { label = "other", description = "其它" }]
```

- `policy_version`：对规范化 JSON 取 SHA-256。规范化规则：
  - 内容为 `{"policy_schema":1,"state_shape":["title","body"],"max_state_chars":N,"questions":[…]}`；
  - 对象键排序，数组保持配置顺序，无空白，UTF-8；
  - 每个问题只含 id / type / instructions / 选项或档位，未填的可选项省略。

  改问题即产生新版本。provider、model、`laya_max_len` 属于执行参数，不进 hash，由 Phase B 与结果一起记录。
  `laya_max_len` 会改变模型实际看到的长度，所以 Phase B 同时记录 `usage.truncated`。
- 旧配置不含这两段时照常加载（serde 缺省）。validate-config 规则：
  - 有 `[decision]` 段：按所选 provider 的 profile 校验每个板块的问题。enabled = false 也校验，问题校验不碰网络。
  - 无 `[decision]` 段：只做公共校验。
  - 板块写了问题、但 `[decision]` 缺失或 enabled = false：给出非致命 warning（code `decision_inert`，带
    `category`，与 `prompt_template` warning 同一模式）。
  - **仅在 enabled 时**检查 `api_key_env` 指向的变量是否存在，不读取、不回显其值。
  - Jev 使用 `jev-latest` / `jev-preview` 别名时给出 warning。

### 7. 分阶段

- **Phase A（本 ADR 批准后实施）**：
  - 交付：decision crate（trait、SystemOne HTTP 实现、两个 profile、请求 / 响应校验、错误）、domain DTO、
    config 解析与校验；
  - 测试：离线契约测试（wiremock 断言请求体的确切形状、两家响应 fixture、各类错误映射、校验失败、脱敏）；
    一个 `#[ignore]` 的真机探针测试，需要 env 变量，Jev / Laya 各发 1 个小请求；
  - 不接入任何 flow，不建表，不加 CLI 子命令。
- **Phase B（另立 ADR）**：独立 `decision-run` 子命令。
  - 自有表（article_id、policy_version、provider、model、state 指纹、是否截断、校验后的答案、
    error_kind、latency、tokens），自有 claim / lease、并发、超时与预算，双方言迁移；
  - 只读文章，不写 `article_ai_results` / `articles.state`，不覆盖 `ai_raw_response`；
  - 关闭时连接都不建立。
- **Phase C（另行决定）**：人工标注评估集与指标；是否让高置信判断参与过滤属于骨架级决策，届时单独提交。

## Consequences

### 正面后果
- provider 差异集中在一个 crate 的 profile 中，核心层只见到经过校验的类型化结果；以后换 provider 或新增
  provider 不改核心。
- 默认关闭、Phase A 不接入流程：生产行为零变化。
- 置信度带来源标签，避免把 Jev 的阈值套到 Laya 上（两者定义不同）。

### 负面后果 / 代价
- 新增一个 workspace crate 与一组配置段（公开配置契约扩大，需版本责任）。
- Jev 对中文的准确度官方自称较低；Laya 默认截断 1024 token。两者都要在 Phase C 用真实数据验证，
  Phase A 只保证契约正确。
- 真机探针依赖用户提供的 Jev key 与可达的 Laya 实例，不进 CI。
- 数据外发：Jev 为托管服务，启用后文章标题与截断后的正文会发往 TypeSafe，与现在发往 OpenAI 兼容端点是同一类外发；
  Laya 自部署不出本机网络。
- 费用：Jev 按输入 token 计费（$0.042 / 百万 token，输出免费），实际以账单为准。

### 后续行动
- 批准后实施 Phase A；同步 plan/18、plan/06、plan/13（说明 typed-decision 不是模型路由）、地图。
- Phase B 设计（decision-run、表结构、lease 预算）单独提交 ADR。

## Links

- [plan/18-typed-decision.md](../plan/18-typed-decision.md)
- [plan/13-non-goals.md](../plan/13-non-goals.md) §5、§8
- Jev：https://docs.typesafe.ai/api 、/models 、/confidence 、/primitives/score
- laya-server：https://github.com/1Panel-dev/laya-server/tree/434e9b350f29a88afd773aee99ca01ee32fffb24
- Laya：https://github.com/NandhaKishorM/laya/tree/6d942c92081fbc139e736bbd9ac0023223c29b7f
