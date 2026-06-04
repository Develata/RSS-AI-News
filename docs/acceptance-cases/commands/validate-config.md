# AC-C-05: validate-config 子命令

## 功能描述

加载 `.env` + `app.toml` + `categories/*.toml`，执行 structural / general / command-specific 三阶段校验。
对合法 config 返回 exit 0；任何 ConfigError 返回 exit 2。

面向场景：CI 静态保险（避免坏 config 进入 main）、部署侧前置门禁、本地修改 config 自检。

## 验收标准

### 命中条件（success path）

- example 配置（`configs/app.toml.example` + `configs/categories/*.toml.example`）通过
- `[ai].enabled=true` + `OPENAI_API_KEY` 存在 → 通过
- `[ai].enabled=false` + 无 `OPENAI_API_KEY` → 通过
- 远端 publish 启用 + `GITHUB_TOKEN` 存在 → 通过
- `--local-only` 豁免 `GITHUB_TOKEN` → 通过
- `{RSSHUB}` / `{RSSHUB_BASE_URL}` 占位符在 `RSSHUB_BASE_URL` 提供时通过
- structural 阶段忽略 env 缺失（让 migrate 等基础设施命令可独立运行）
- `min_importance_score = 0` 被识别为"显式无下限"，与 `None` 区分

### 失败条件（failure path）

- `schema_version != "1"` → exit 2
- `[ai].enabled=true` + 缺 `OPENAI_API_KEY` → exit 2
- 远端 publish + 缺 `GITHUB_TOKEN`（无 `--local-only`） → exit 2
- 重复分类 key → exit 2
- 非法 `feed_url` → exit 2
- `{RSSHUB}` / 别名 + 缺 `RSSHUB_BASE_URL` → exit 2
- `path_template` 含 `..` / 反斜杠 / 无日期 token → exit 2
- `report_template` 缺 `{items}` / 未知 placeholder / 大括号不匹配 → exit 2
- 跨分类 path 冲突 → exit 2
- `report` 一次性输出**全部** ConfigError（diagnostic 列表），不首错即停

## 测试覆盖

| 测试名 | 路径 | 覆盖标准 |
|---|---|---|
| `args_parsing_parses_validate_config` | `crates/cli/tests/args_parsing_tests.rs` | args 解析 |
| `validate_config_cmd_valid_config_returns_success` | `crates/cli/tests/validate_config_cmd_tests.rs` | happy path |
| `validate_config_cmd_invalid_config_returns_config_error` | 同上 | 结构问题 |
| `validate_config_cmd_missing_env_with_ai_enabled_returns_config_error` | 同上 | env 缺失 |
| `load_example_configs_end_to_end` | `crates/config/tests/load_examples.rs` | example 端到端 |

（其余字段级校验项见 [../pipelines/06-config-loading.md](../pipelines/06-config-loading.md) 的完整测试矩阵，本 case 仅覆盖 CLI 入口。）

## 当前状态

`passing`

## 相关文档

- 设计：[../../plan/06-config.md](../../plan/06-config.md) §10 validate-config
- 配置加载验收：[../pipelines/06-config-loading.md](../pipelines/06-config-loading.md)
- 错误模型：[../../plan/11-error-and-recovery.md](../../plan/11-error-and-recovery.md) §Exit Code
