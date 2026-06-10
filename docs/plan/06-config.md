# 06 — 配置

本章详解三层配置体系：`.env`（密钥）/ `app.toml`（全局）/ `categories/*.toml`（分类）。

配置是宪法 §3.4 单一真相源在"运行时输入"维度的体现：除 CLI 覆盖项外，所有行为都由这三类文件决定。

## 1. 边界

本章覆盖：
- 三层文件的字段范围与解析顺序
- `schema_version` 校验
- RSSHub 占位符 `{RSSHUB}` / `{RSSHUB_BASE_URL}` 运行时展开
- `[ai].enabled × [publish].include_unscored` 真值表
- CLI 覆盖（`CliOverrides`）的有效字段
- `validate-config` 子命令的两类检查
- `config_sha256` 作为版本指纹

**不覆盖**：
- 配置如何注入 RunContext → [./09-cli-and-runtime.md](./09-cli-and-runtime.md)
- 各能力章节内的字段语义细节 → 各章
- rule_versions 表与版本化驱动 → [./05-storage.md](./05-storage.md) §8
- 部署侧（compose/Docker 注入 env）→ [./12-deployment.md](./12-deployment.md)

## 2. 三层文件分工

| 文件 | 作用域 | 含密钥？ | 加载 |
|---|---|---|---|
| `.env`（或 `--env-file`） | 全局密钥与外部端点 | 是 | `crates/config/src/env.rs::load` |
| `app.toml`（`--config-dir/app.toml`） | 全局非密钥设置 | 否 | `crates/config/src/loader.rs::load_inner` |
| `categories/*.toml` | 每个分类的 sources + override | 否（rsshub_access_key 例外） | 同上 |

加载顺序：env → app → categories → 合并 `CliOverrides` → 计算 `config_sha256` → `validate::*`。

## 3. `.env` 字段

固定字段由 `EnvConfig` 显式枚举；此外 `.env` 文件**全量键值会被保留**（私有、Debug redact），
供板块 `api_key_env` 动态解析（`EnvConfig::resolve_secret(name)`，W14-B，
见 [./14-ai-fallback.md](./14-ai-fallback.md) §B.2）。**进程环境不被注入**。

```rust
pub struct EnvConfig {
    pub openai_api_key: Option<SecretString>,
    pub openai_base_url: Option<String>,
    pub github_token: Option<SecretString>,
    pub rsshub_base_url: Option<String>,
    pub rsshub_access_key: Option<SecretString>,  // 全局 RSSHub key fallback
    pub http_proxy: Option<String>,
    pub https_proxy: Option<String>,
    pub database_url: Option<String>,             // 优先于 [database].sqlite_path
}
```

加载规则（`env.rs::value`）：
1. 优先读进程环境变量
2. 否则读 `.env` 文件（`dotenvy::from_path_iter`），同 key 多次出现取最后一次
3. 空字符串视为未设置

密钥字段（`*_api_key` / `*_token` / `*_access_key`）包装为 `SecretString`，`Debug` 输出固定为 `***`，
防止 tracing/log 整体格式化时泄漏（见 `env.rs::tests::env_secret_fields_redact_in_debug_output`）。

## 4. `app.toml` 字段

由 `AppConfig` 严格定义。完整字段见 [`crates/config/src/app.rs`](../../crates/config/src/app.rs)，
分段如下：

| 段 | 字段范围 | 必需 |
|---|---|---|
| 顶层 | `schema_version`（必须 = `"1"`） | 是 |
| `[database]` | `driver` ∈ {`sqlite`, `postgres`} / `sqlite_path` / `max_connections` / `busy_timeout_ms` | 是 |
| `[http]` | UA / 超时 / 重试 / 并发数 | 是 |
| `[ai]` | `enabled` / 模型 / token / 温度 / `[ai.rate_limit]` | 是 |
| `[publish]` | GitHub 目标 / `local_output_dir` / `include_unscored` / `max_items_per_report`（`NonZeroU32`）/ `min_importance_score`（`Score0To100`）/ `candidate_window_hours` | 是 |
| `[publish.template]` | `path_template` / `frontmatter_template` / `report_template` / `item_template` | 是 |
| `[dedup]` | 三层去重开关 + `link_normalizer_version` | 是 |
| `[extractor]` | `strategy_order` / `max_body_bytes` / `min_body_chars` | 是 |
| `[lease]` | fetch / ai / publish 三个 lease 时长 + 回收间隔 | 是 |
| `[retry]` | feed / ai / publish 各自 `max_attempts` | 是 |
| `[runtime]` | `max_batches_per_run`（默认 10；`0` = 不限） | **可缺省** |
| `[artifact]` | `retention_policy` / `sample_rate` / `inline_threshold_bytes` / `file_storage_dir` / `ttl_days` | 是 |
| `[observability]` | `log_level` / `log_format` / `log_file` / `enable_metrics` / `metrics_bind` | 是 |

类型层不变量（写在结构体上而非运行时校验）：
- `max_items_per_report: NonZeroU32` — `0` 在反序列化阶段直接失败（避免 `LIMIT 0` 静默丢数据）
- `min_importance_score: Score0To100` — 越界值在反序列化阶段失败
- `runtime` 用 `#[serde(default)]`：旧 `app.toml` 无 `[runtime]` 段时仍能解析（`tests::missing_runtime_block_falls_back_to_default`）

## 5. `categories/*.toml` 字段

每个文件代表一个分类。由 `CategoryConfig` 定义，结构：

```toml
schema_version = "1"

[category]
key = "ai"
display_name = "AI"
priority = 10

[category.ai_override]   # 可选：覆盖 [ai] 的子集
prompt_template = "..."
max_input_chars = 12000
model = "gpt-4o"         # 空串 "" = 继承全局 [ai].model（trim 后为空即继承）
fallback_models = ["gpt-4o-mini", "deepseek-chat"]  # 可选：覆盖全局 fallback 链。
                         # 省略(None)=继承全局；[]=显式禁用；非空=覆盖。见 ./14-ai-fallback.md
base_url = "https://api.deepseek.com/v1"  # 可选：板块独立 endpoint。空/省略 = 继承
                         # 全局 OPENAI_BASE_URL。见 ./14-ai-fallback.md §B（W14-B）
api_key_env = "DEEPSEEK_API_KEY"          # 可选：板块独立 key 的 env 变量名引用
                         # （key 本身绝不入 toml）。空/省略 = 继承全局 OPENAI_API_KEY

[category.publish_override]   # 可选：覆盖 [publish] 的子集
max_items_per_report = 50
min_importance_score = 0     # 0 = 显式无下限（None 才表示继承）
include_unscored = true
path_template = "custom/{category_key}/{YYYY}/{YYYYMMDD}.md"

[[sources]]
key = "openai_blog"
display_name = "OpenAI"
feed_url = "{RSSHUB}/openai/blog"
feed_kind = "rsshub"            # rss | atom | json_feed | rsshub
priority = 10
enabled = true
```

`PublishOverride` 字段全部 `Option`，**`None` = 继承全局**，`Some(...)` = 覆盖。
`min_importance_score` 用 `Option<Score0To100>` 而非 `Option<u8>`：W2-B-1 旧实现把 `try_new` 失败
折叠为继承默认，掩盖配置错误，现已在反序列化层硬失败（见 `category.rs` 注释）。

## 6. CLI 覆盖

`CliOverrides`（[`crates/config/src/overrides.rs`](../../crates/config/src/overrides.rs)）承载
**全局**可被 CLI flag 覆盖的字段，由 `apply_to_app` 写回 `AppConfig`：

| 字段 | 覆盖目标 | 来源 flag |
|---|---|---|
| `db_path: Option<PathBuf>` | `database.sqlite_path` | `--db-path` |
| `log_level: Option<String>` | `observability.log_level` | `--log-level` |
| `log_format: Option<String>` | `observability.log_format` | `--log-format` |
| `timezone: Option<String>` | `publish.target_timezone` | `--timezone` |
| `category_filter: Option<String>` | `categories_filtered()` 过滤器（不写入 app） | `--category` |
| `dry_run: bool` | 运行期 sentinel（不写入 app） | `--dry-run` |
| `max_batches: Option<u32>` | `runtime.max_batches_per_run`（`Some(0)` 表示不限） | `--max-batches` |

**子命令级**参数（如 `publish --local-only`、`ai-run --batch-size`、`run --ai-off` 等）
由各子命令在 args 结构体中单独读取，**不进入** `CliOverrides`、**不**参与 `config_sha256`。

`CliOverrides` 全局字段在 `loader::load_inner` 中通过 `apply_to_app` 写入 `AppConfig`，
随后整个 `CliOverrides` 仍保留在 `LoadedConfig.cli_overrides` 供下游引用。
`config_sha256` 的输入是**原始 toml 文本**（`app.toml` + `categories/*.toml`），
**不**涉及 override 后的 `AppConfig` 字段，因此 CLI 临时覆盖不会影响指纹。

## 7. RSSHub 占位符

`feed_url` 中的 `{RSSHUB}` 与 `{RSSHUB_BASE_URL}` 是**两个等价别名**，运行时由
`crates/config/src/rsshub.rs::expand_base_placeholders` 替换为 `EnvConfig.rsshub_base_url`，
末尾斜杠会被 `trim_end_matches('/')` 清理。

校验侧：当 source URL 含任一占位符，必须设置 `RSSHUB_BASE_URL`，否则
`run_general_checks` 报错（`validate.rs::tests::rsshub_placeholder_without_base_url_fails`）。

设计原因：让 categories/*.toml 可入仓不携带部署信息，部署侧只需在 `.env` 注入一次。

每 source 还可单独配置 `rsshub_access_key`，落到 `SourceSecrets`（`loader::SourceSecrets`），
不暴露到 `CategoryConfig.sources[i]` 字段，避免日志/Debug 泄漏。
详见 [../adr/0007-rsshub-secret-runtime-expansion.md](../adr/0007-rsshub-secret-runtime-expansion.md)。

## 8. `[ai].enabled × [publish].include_unscored` 真值表

| `ai.enabled` | `include_unscored` | 行为 |
|:---:|:---:|---|
| true | false | 默认。AI 必跑；评分低于 `min_importance_score` 的条目被过滤 |
| true | true | AI 必跑；评分缺失也允许进入候选（仍受 `min_importance_score` 约束） |
| false | true | AI-off 直通：`articles` 跳过 AI 阶段直接进入发布候选 |
| false | false | **运行 publish 时报错**：没有评分又不允许 unscored，候选必为空 |

校验由 `validate::checks::collect_publish_checks` 完成。AI-off 直通的实现路径详见
[./03-ai.md](./03-ai.md) §AI-off 模式。

## 9. `schema_version` 校验

`SUPPORTED_SCHEMA_VERSION = "1"`（`validate.rs`）。当前**只支持单一版本**；不匹配直接
`ConfigError::ValidationFailed`。版本升级流程：

1. 引入 `"2"` 时新增字段或破坏性变更
2. 在 `validate.rs::run_general_checks` 增加迁移分支
3. 配套写 ADR 留痕
4. 旧用户须手动 `schema_version = "2"` 后重新运行 `validate-config`

未来若引入 schema 迁移会按上述路径走。当前**没有**自动升级 toml 文件的逻辑。

## 10. `validate-config` 子命令

入口：[`crates/cli/src/commands/validate_config.rs`](../../crates/cli/src/commands/validate_config.rs)。

执行的检查分三类：

| 检查 | 函数 | 触发场景 |
|---|---|---|
| 结构性检查 | `validate::run_structural_checks` | `migrate` 等不依赖密钥的子命令 |
| 通用检查（结构 + env 必备） | `validate::run_general_checks` | 默认 `validate-config` |
| 命令专属检查 | `validate::run_command_checks(CommandKind::*)` | Publish / AiRun / Doctor 各自的额外门槛 |

`CommandKind::Doctor` **不**把"ai-run 与当前配置不兼容"视为错误（`ai.enabled=false` 是合法状态），
仅在 `CommandKind::AiRun` 分支单独拦截，返回 `ConfigError::AiRunWhileDisabled`。

诊断结构 `DiagnosticReport` 收集所有违例，**一次性**报告，而非首错即停。
错误格式见 [`crates/config/src/error.rs`](../../crates/config/src/error.rs)。

## 11. `config_sha256` 与版本指纹

`compute_config_sha256(app_toml, &[(name, content), ...])`（`version.rs`）按
`app.toml::<内容>\ncategories/<name>::<内容>\n` 拼接，对 `categories` 按 name 排序后取 SHA-256。

- 输入相同 → sha 相同（`tests::same_input_produces_same_sha256`）
- categories 顺序变化不影响 sha（先排序）
- env 变量**不**参与（密钥变化不应触发版本切换）

`config_sha256` 写入 `rule_versions` 表，用于 `ConfigVersionStore::get_or_create_config_version`，
向 reindex 流程关联具体配置快照。详见 [./05-storage.md](./05-storage.md) §8 与
[../adr/0004-active-rule-resolver-partial-unique.md](../adr/0004-active-rule-resolver-partial-unique.md)。

> **已知缺口**：bootstrap rule 升 active 后真实 config sha 的替换路径，详见
> Phase B1 结束前未关闭的 W10 后续设计任务。

## 12. 错误模型对接

`ConfigError`（[`crates/config/src/error.rs`](../../crates/config/src/error.rs)）→
`AppError::Config` → exit code `2`（输入错误，详见 [./11-error-and-recovery.md](./11-error-and-recovery.md) §Exit Code 表）。

校验阶段失败一律走 `ValidationFailed { report: DiagnosticReport }`，CLI 层负责将
diagnostic 列表渲染到 stderr。

## 13. 当前实现入口

| 内容 | 路径 |
|---|---|
| AppConfig 结构 | [`crates/config/src/app.rs`](../../crates/config/src/app.rs) |
| CategoryConfig | [`crates/config/src/category.rs`](../../crates/config/src/category.rs) |
| EnvConfig + 密钥 redaction | [`crates/config/src/env.rs`](../../crates/config/src/env.rs) |
| 加载主流程 | [`crates/config/src/loader.rs`](../../crates/config/src/loader.rs) |
| EffectiveConfig 合并 | [`crates/config/src/effective.rs`](../../crates/config/src/effective.rs) |
| CliOverrides | [`crates/config/src/overrides.rs`](../../crates/config/src/overrides.rs) |
| RSSHub 占位符展开 | [`crates/config/src/rsshub.rs`](../../crates/config/src/rsshub.rs) |
| Validation | [`crates/config/src/validate.rs`](../../crates/config/src/validate.rs) + `validate/checks.rs` |
| ConfigError / Diagnostic | [`crates/config/src/error.rs`](../../crates/config/src/error.rs) |
| config_sha256 | [`crates/config/src/version.rs`](../../crates/config/src/version.rs) |
| validate-config CLI | [`crates/cli/src/commands/validate_config.rs`](../../crates/cli/src/commands/validate_config.rs) |

代码路径过时时在 [../map/architecture-diff.md](../map/architecture-diff.md) 登记漂移。
