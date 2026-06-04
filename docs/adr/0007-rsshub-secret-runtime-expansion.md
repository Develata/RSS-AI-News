# ADR 0007: RSSHub 占位符运行时展开 + access_key 不入持久化 URL

- 日期：2026-05（F15 batch 期间）
- 状态：`accepted`
- 决策者：项目主作者

## Context

`categories/*.toml` 中可以配置 RSSHub source，例如：

```toml
[[sources]]
key = "openai_blog"
feed_url = "https://rsshub.example.com/openai/blog"
feed_kind = "rsshub"
```

部署侧通常希望：
1. `categories/*.toml` 入仓但**不带部署信息**（不同环境共享同一份分类配置）
2. RSSHub 实例的 base URL 可在 `.env` 中一次注入，所有 source 自动展开
3. 部分 RSSHub 实例需要 `access_key` —— **密钥不应出现在持久化的 feed_url 中**

候选方案：
- (a) feed_url 写死完整 URL（含 base + access_key）→ 密钥泄漏 + 配置不可入仓
- (b) **`{RSSHUB}` / `{RSSHUB_BASE_URL}` 占位符 + 运行时展开 + access_key 单独存 SourceSecrets**
- (c) categories/*.toml 完全去 URL，只留 path，base 写在 app.toml → 与现有"feed_url 是完整 URL"的设计断层

## Decision

采用 **(b) 占位符 + 运行时展开**：

- feed_url 中允许 `{RSSHUB}` 或 `{RSSHUB_BASE_URL}` 占位符（两者是等价别名）
- 运行时由 `rss_ai_news_config::rsshub::expand_base_placeholders` 替换为 `EnvConfig.rsshub_base_url`
- base URL 的末尾斜杠会被 `trim_end_matches('/')` 清理
- access_key 单独走 `SourceSecrets`（per-source 或全局 `RSSHUB_ACCESS_KEY` env），**不写回**
  `CategoryConfig.sources[i].feed_url` —— 持久化的 feed_url 保持不含密钥
- 校验侧：若 source URL 含占位符，必须设置 `RSSHUB_BASE_URL`，否则 validate 报错

## Consequences

### 正面后果

- categories/*.toml 入仓不带部署信息，**多环境共享同一套分类配置**
- 密钥不会出现在 `feed_url` 中 → 不被 tracing / log / metrics 通过 URL 泄漏
- `EnvConfig` Debug 实现把 `*_access_key: SecretString` 渲染为 `***` —— 双重保险
- 两个等价占位符 `{RSSHUB}` 和 `{RSSHUB_BASE_URL}` 容忍命名习惯差异（W2 fix6 兼容老配置）

### 负面后果 / 代价

- 多了一层间接：调试 source 时要意识到"feed_url 是模板，不是最终 URL"
- expand 在 ingest hot path 上每次都做一次字符串 replace —— 微小性能开销
- 占位符语法是项目自定义的（非标准 .env 风格）—— 新人需读文档才知道

### 后续行动

- 占位符集合**封闭**：当前只支持 `{RSSHUB}` 与 `{RSSHUB_BASE_URL}`，不扩展到任意环境变量替换
  （避免变成 Helm / Jinja 那样的模板语言）
- 若未来需要其它 secret 注入（如 HTTP Basic Auth），走 SourceSecrets per-source 字段，不扩展占位符
- 测试覆盖：见 `rsshub_placeholder_without_base_url_fails` / `rsshub_base_url_placeholder_alias_*` 系列

## Links

- 设计：[../plan/06-config.md](../plan/06-config.md) §7 RSSHub 占位符
- 实现：[`crates/config/src/rsshub.rs`](../../crates/config/src/rsshub.rs)、[`crates/config/src/loader.rs`](../../crates/config/src/loader.rs)（SourceSecrets）
- 验证：[`crates/config/src/validate.rs`](../../crates/config/src/validate.rs) RSSHub 相关 check
- 验收：[../acceptance-cases/pipelines/06-config-loading.md](../acceptance-cases/pipelines/06-config-loading.md)、[../acceptance-cases/pipelines/01-feed-ingest.md](../acceptance-cases/pipelines/01-feed-ingest.md)
- 相关 commits：`6b29024` (refactor: separate RSSHub source secrets from config schema)、
  `e2db65b` (fix: keep RSSHub access key out of persisted URLs)、
  `4b9e128` (fix: accept RSSHUB_BASE_URL placeholder alias)、
  `3e52353` (fix: align RSSHub placeholder diagnostics)
