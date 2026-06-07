# 14 — AI 失败回退（fallback 模型链）+ 板块凭证自治

本章定义 AI 调用失败时的**模型回退**能力，以及板块（category）独立凭证的设计。
解禁 [./13-non-goals.md](./13-non-goals.md) §5.2 中"多模型 fallback chain"这一**单一**子项；
"按长度路由""智能选择模型""运行时跨 provider 动态路由"**仍为 non-goal**。

> 核心区分：fallback 是"**失败后的静态顺序降级**"（同输入、主模型挂了按固定顺序换下一个），
> 不是"按输入挑模型"。前者不引入"模型路由 essence"，后者引入——故只做前者。

## 1. 分期

| 期 | 内容 | 凭证 | 触及 |
|---|---|---|---|
| **A（本期）** | 全局 + 板块的**模型** fallback 链 | 沿用现有**全局单凭证**（单 key / base / client） | ai 错误分类 + runtime 编排 + config 加字段 + storage 加一列 |
| **B（后续）** | 板块独立**凭证**自治（base_url + key + model + fallback 各板块一套，留空继承全局） | 每板块可独立 | config 加载 / 校验骨架 + composition root client 装配 |

B 的难点在 config 层（现 `config::load` 强制全局 `OPENAI_API_KEY`/`OPENAI_BASE_URL`、`.env` 原始键值
load 后丢弃、不注入 `std::env`），需把动态 env 解析做成 config 层一等能力、校验按 selected category
判断。B 单独立项，本章 §B 仅占位。

## 2. 语义（A 期）

- **一次"模型尝试" = invoke + 写 raw_artifact + parse + schema/outcome 校验整段**。任一步失败
  且该错误"换模型可能有救"，即在**同一次执行内**试下一个模型。
  （内容类错误 `InvalidJson`/`MissingField` 发生在 invoke 成功之后的 parse 阶段，故循环必须包住整段，
  而非只包 `invoke()`——见 [`crates/runtime/src/flows/ai_run.rs`](../../crates/runtime/src/flows/ai_run.rs) `process_one`。）
- **模型链** = `[主模型, ...fallback_models]`，去重、跳过等于主模型者。
- **主模型锚定** `article_ai_results.model_id`：来自 claim 出的行（`ClaimedAiResult.model_id`），
  是幂等键 `ON CONFLICT(article_id, prompt_version, output_schema_version, model_id)` 的一部分，
  **不可变**。process **不**用 `opts.model_id`（修既有缺陷：配置改 model 后会用新 model 调旧行）。
- **实际成功模型** 写非键列 `effective_model_id`（见 §4），不进幂等键。
- **链尽 / 遇到不可回退错误后**：按最后错误的 `is_retryable()` 走现有
  `release_retryable_ai_failure`（回 `pending`）/ `release_permanent_ai_failure`。
- **正交性**：横向 fallback（换模型，单次执行内）与纵向 `[retry].ai_max_attempts`（回队重试）独立；
  一次 attempt 最多试 `1 + len(fallback)` 个模型，失败回队消耗 1 个 attempt，下次再走一遍链。

## 3. 触发条件 — `AiError::should_fallback()`

落在 [`crates/ai/src/error.rs`](../../crates/ai/src/error.rs)，与 `is_retryable()`/`error_kind()` 并列。
策略：**除两类"换 model 名 100% 无救"的错误外，全部触发**（用户口径："只要失败就 fallback"）。

| should_fallback | 错误 | 理由 |
|---|---|---|
| **false** | `InvalidConfig` | key 空 / base 非法 URL，与 model 名无关，全局共用凭证下换模型必同样失败 |
| **false** | `ConnectionFailed` | 连不上 endpoint，同 base_url 换 model 名照样连不上 |
| **true** | 其余全部 | `QuotaExceeded` / `RateLimited` / `HttpStatus`(任意) / `ModelUnavailable` / `HttpTimeout` / `InvalidJson` / `MissingField` / `InvalidFieldValue` / `EmptyResponse` |

> 注：`QuotaExceeded` 在同一 key 下多为账户级、换模型未必有救，但用户选择"只要失败就试"，
> 故纳入。A 期全局单凭证，这两个 false 排除是纯逻辑的（非策略偏好）。

新增 `AiError::ModelUnavailable { message }`：在 `classify_error_response`（reqwest 路径）与
`From<OpenAIError::ApiError>`（async-openai 路径）**双路径**识别 `model_not_found` /
`model_not_available` / `does not exist`（400/404、JSON 与纯文本）。同时修正 `From<OpenAIError::ApiError>`
把非 quota/rate 的 API error 粗归 `ConnectionFailed` 的问题（影响 fallback 判定）。

## 4. `effective_model_id`（storage）

`article_ai_results` 加 **nullable** 列 `effective_model_id TEXT`（sqlite + postgres 双方言 migration，带 rollback）：

- 迁移策略：新增 nullable 列 → **一次性回填**既有行 `effective_model_id = model_id` → 此后成功 release 必写实际模型。
  （不能用 column default 引用同行 `model_id`，故用一次性 UPDATE 回填。）
- `NewAiResult` 不变（创建 pending 行只写 `model_id` = 主模型）。
- `AiSuccessOutcome` / `release_success_and_advance_article` 签名加 `effective_model_id`，sqlite/pg 双实现写入。
- 用途：板块按**实际**模型做质量 / 成本统计、审计。`model_id` 仍是"幂等主模型"身份。

## 5. lease 边界

fallback 把一次 attempt 从 1 次 HTTP 变成最多 `1+len(fallback)` 次；又因 process 是
**整批 claim（一次拿一批 lease）→ Semaphore 限并发执行**，后排任务排队也耗 lease。故：

- **运行前**（组装完实际 `batch_size`〔CLI `--batch-size`〕、并发〔`app.http.concurrent_fetches`〕、
  chain 长度后）fail-fast 校验：
  `ceil(batch_size / concurrent) × request_timeout_seconds × (1 + len(fallback)) ≤ lease.ai_duration_seconds`。
  （不能只做配置期静态校验——batch_size 是运行时 CLI 参数。）
- 每次 fallback 前校验 lease 仍持有（`assert_lease_held`），防 lease 过期被 reclaim 后另一 worker
  重复 claim 导致重复扣费 / release 冲突。

## 6. 可观测性

- 每次降级 emit run_event，记录完整 chain：`primary_model_id` + 每个 `attempted_model_id` / `error_kind`
  / `should_fallback` + `actual_model_id` 或 `final_error_kind`。run_event 是 best-effort 旁路
  （写失败不阻塞主流程）；强一致审计走 `effective_model_id` 列。
- **每次失败尝试各留一份 raw_artifact**：key 含 attempt/model，不互相覆盖（现 key 只含 ai_result_id + upsert
  会覆盖）。遵循现有 retention / sample 策略。
- 凭证（key）不入 run_event / 日志（`SecretString` 自带 redaction）。

## 7. 配置（A 期）

```toml
# app.toml
[ai]
model = "gpt-4o-mini"
fallback_models = ["gpt-4o", "deepseek-chat"]   # 新增，#[serde(default)]，空 = 行为不变

# categories/<key>.toml
[category.ai_override]
model = "gpt-4o"                                  # 空串 "" = 继承全局（trim 后为空即继承）
fallback_models = ["gpt-4o-mini"]                # 省略(None)=继承全局 / []=禁用 / 非空=覆盖
```

折叠在 [`crates/config/src/effective.rs`](../../crates/config/src/effective.rs)：
- `model`：override 非空（trim 后）> 全局
- `fallback_models`：`None`=继承全局 / `Some([])`=禁用 / `Some(非空)`=覆盖；元素 trim、非空白、与主模型去重

**主模型解析修正**（既有缺陷）：`ai-run` 走 `effective.model` 再套 CLI `--model`
（`--model` > category > global），不再直接 `ai_override.model.clone()`（会把示例里 `model=""` 当真实模型）。
**backfill**：候选文章经 `list_in_window_for_backfill` **不按 category 过滤**（跨 category 全局回填），
故 A 期 backfill **仍只用全局 model + 全局 fallback**，不引入 category model。

## 8. 第一期 A 实现阶段

1. **P0** 契约（本章 + 13-non-goals §5.2 + 03-ai §2.3/§9 + 06-config 示例）
2. **P1** config：字段 + 折叠 + 主模型解析修正 + validate（元素非空白 / 去重）+ 测试
3. **P2** ai：`ModelUnavailable` + `should_fallback` + 双路径识别 + 测试
4. **P3** storage：`effective_model_id` 列 + 双方言 migration + outcome/release 签名 + 双实现 + 测试
5. **P4** runtime：`process_one` 整段尝试循环 + `AiRunOptions.fallback_models` + CLI 装配 + artifact key
   + chain 可观测 + lease 运行前校验 + 测试
6. **P5** 收尾：示例 toml 注释 + doctor chain 摘要（可选）+ 全量回归（含 docker PG 双方言）

关键阶段（P3 migration、P4 编排）实现后用 codex review 真实 diff。

## 9. 不变契约（铁律）

- `model_id` 幂等键不可变；fallback 成功写 `effective_model_id`，不碰 `model_id`。
- SQL 字符串改动谨慎（双方言 byte 级对齐 storage 既有约定，见 [./05-storage.md](./05-storage.md)）。
- 不做任何"按输入挑模型"的智能路由（§5.2 仍排除）。
- A 期不动 config 加载骨架、不动 crate 边界。

## B. 第二期：板块凭证自治（占位）

板块可独立配 `base_url` + `api_key_env`（env 变量名引用，**绝不**明文入 toml / 库）+ model + fallback，
留空继承全局。因一次 ai-run 严格单 category（task_gen + claim 均按 `category_key` 过滤），凭证可在
composition root 按 selected category **静态解析**为单 client，**无需运行时路由**。
需先重构 config 加载 / 校验（见 §1）。单独立项，本期不做。
