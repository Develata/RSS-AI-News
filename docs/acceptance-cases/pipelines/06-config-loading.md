# AC-P-06: 配置加载与校验

## 功能描述

`crates/config` 的三层加载（`.env` → `app.toml` → `categories/*.toml`）→ CLI overrides 合并 →
SHA-256 指纹计算 → 多阶段校验（structural / general / command-specific）。

所有 CLI 子命令在主流程开始前必须通过相应的校验门，校验失败一律以 exit code `78`（ConfigError，sysexits `EX_CONFIG`）退出。

面向场景：每次 CLI 启动；显式 `validate-config` / `doctor` 子命令；CI 静态保险。

## 验收标准

### 命中条件（success path）

- `configs/app.toml.example` + `configs/categories/*.toml.example` 端到端加载通过
- 旧 `app.toml`（无 `[runtime]` 段）走 `#[serde(default)]` 平滑兼容
- `max_batches_per_run = 0` 反序列化保留为 0（表示"不限"）
- `--max-batches N` CLI flag 覆盖 `runtime.max_batches_per_run`；`Some(0)` 透传
- `--max-batches` 仅在支持的子命令（ingest / ai-run / run）上被接受；其它子命令拒绝
- `{RSSHUB}` / `{RSSHUB_BASE_URL}` 两个别名都触发 RSSHub base 必填校验，都能在 `RSSHUB_BASE_URL` 环境变量提供时正确展开
- `validate-config` 对合法 config 返回成功；对结构问题、env 缺失、AI 矛盾都给出明确 `ConfigError`
- `path_template` 含 `{YYYY}` / `{YYYYMMDD}` / `{YYYY}/{MM}/{DD}` 等日期 token 时通过；分类级 override 允许省略 `{category_key}` 但必须有日期
- 跨分类 path 冲突检测：当多个分类的渲染样本路径完全相同时 `validate` 拦下（即使一个分类继承全局、一个用 override）
- 模板 placeholder 集是封闭白名单：拼错的 placeholder（如 `titel_md`）必须立即失败
- `min_importance_score = Some(0)` 表示**显式无下限**，与 `None` 继承全局**严格区分**
- `EnvConfig` 的 `Debug` 输出永远把密钥替换为 `***`，不泄漏 raw 值

### 失败条件（failure path）

- `schema_version != "1"` → `ConfigError::ValidationFailed`
- `[ai].enabled=true` 且 `OPENAI_API_KEY` 缺失 → `ValidationFailed`
- `categories/` 中 key 重复 → `ValidationFailed`
- `feed_url` 非合法 URL → `ValidationFailed`
- 远端 publish（owner+repo 非空）缺 `GITHUB_TOKEN` 且没传 `--local-only` → `ValidationFailed`
- `[ai].enabled=false` 时 `ai-run` 子命令 → `ConfigError::AiRunWhileDisabled`
- `path_template` 含 `..` / 反斜杠 / 无日期 token → `ValidationFailed`
- `report_template` 缺 `{items}` / 模板含未知 placeholder / 大括号不匹配 → `ValidationFailed`
- doctor 子命令**不**把 `ai.enabled=false` 视为失败（仅 ai-run 分支才拦）

## 测试覆盖

| 测试名 | 路径 | 覆盖标准 |
|---|---|---|
| `parses_example_app_config` | `crates/config/src/app.rs` (#[cfg(test)]) | example 配置端到端 |
| `missing_runtime_block_falls_back_to_default` | 同上 | 旧 toml 兼容 |
| `runtime_max_batches_zero_round_trips_as_unlimited` | 同上 | `0` 透传 |
| `load_example_configs_end_to_end` | `crates/config/tests/load_examples.rs` | 端到端加载 |
| `env_file_loads_non_empty_values` | `crates/config/src/env.rs` | .env 加载 |
| `env_secret_fields_redact_in_debug_output` | 同上 | 密钥 redaction |
| `missing_openai_key_with_ai_enabled_fails` | `crates/config/src/validate.rs` | AI on 缺 key |
| `missing_openai_key_with_ai_disabled_is_ok` | 同上 | AI off 不要求 key |
| `missing_github_token_for_remote_publish_fails` | 同上 | 远端缺 token |
| `missing_github_token_with_local_only_is_ok` | 同上 | local-only 豁免 |
| `ai_run_with_ai_disabled_returns_specific_error` | 同上 | AiRunWhileDisabled |
| `rsshub_placeholder_without_base_url_fails` | 同上 | `{RSSHUB}` 缺 base |
| `rsshub_base_url_placeholder_alias_without_base_url_fails` | 同上 | `{RSSHUB_BASE_URL}` 缺 base |
| `rsshub_base_url_placeholder_alias_with_base_url_is_valid` | 同上 | 别名展开 |
| `duplicate_category_key_fails` | 同上 | 重复 key |
| `invalid_feed_url_fails` | 同上 | URL 非法 |
| `unsupported_schema_version_fails` | 同上 | schema 版本 |
| `structural_checks_ignore_missing_openai_env_when_ai_enabled` | 同上 | migrate 结构性检查 |
| `path_template_parent_dir_fails` | 同上 | path 防穿越 |
| `path_template_backslash_fails` | 同上 | 反斜杠拒绝 |
| `path_template_without_date_fails` | 同上 | 缺日期 token |
| `path_template_accepts_split_date_tokens` | 同上 | 拆分日期 token |
| `category_path_template_can_omit_category_token` | 同上 | 分类 override 豁免 |
| `category_path_template_still_requires_date_token` | 同上 | 分类 override 仍要日期 |
| `cross_category_path_collision_fails_when_overrides_collapse_to_same_path` | 同上 | 跨分类冲突 |
| `cross_category_path_with_category_token_does_not_collide` | 同上 | category token 隔离 |
| `cross_category_collision_detected_when_only_one_category_has_override` | 同上 | 全局 vs override 冲突 |
| `report_template_without_items_fails` | 同上 | items placeholder 必填 |
| `template_unknown_placeholder_fails` | 同上 | 未知 placeholder |
| `template_unmatched_brace_fails` | 同上 | 大括号不匹配 |
| `tokens_per_minute_zero_is_valid` | 同上 | 0 合法 |
| `max_batches_some_overrides_runtime_config` | `crates/config/src/overrides.rs` | CLI 覆盖 |
| `max_batches_some_zero_means_unlimited_and_passes_through` | 同上 | 0 透传 |
| `max_batches_none_preserves_config_value` | 同上 | None 不动 |
| `args_parsing_max_batches_rejected_on_unsupporting_subcommand` | `crates/cli/tests/args_parsing_tests.rs` | flag 子命令隔离 |
| `args_parsing_max_batches_flows_into_cli_overrides_from_ingest` | 同上 | flag→overrides 转换 |
| `validate_config_cmd_valid_config_returns_success` | `crates/cli/tests/validate_config_cmd_tests.rs` | CLI happy |
| `validate_config_cmd_invalid_config_returns_config_error` | 同上 | CLI 失败 |
| `validate_config_cmd_missing_env_with_ai_enabled_returns_config_error` | 同上 | CLI env 缺失 |

## 当前状态

`passing`

## 相关文档

- 设计：[../../plan/06-config.md](../../plan/06-config.md)
- 错误模型：[../../plan/11-error-and-recovery.md](../../plan/11-error-and-recovery.md) §Exit Code
- CLI surface：[../../plan/09-cli-and-runtime.md](../../plan/09-cli-and-runtime.md)
- 决策：`../../adr/0007-rsshub-secret-runtime-expansion.md`
